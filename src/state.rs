use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    env, fs,
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use crossterm::{
    event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use serde_json::{Map, Value, json};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    completion::{
        CompletionCandidate, CompletionKind, CompletionMode, CompletionTarget, completion_target,
        completion_text, filter_candidates,
    },
    editor::{ATTACHMENT_PLACEHOLDER, Editor},
    integrations::{
        MarketplacePicker, MarketplacePickerResult, McpPicker, McpPickerResult, McpServerInfo,
        PluginCatalog, PluginDetail, PluginInfo, PluginPicker, PluginPickerResult, PluginScope,
        PluginTarget,
    },
    pricing::{self, CostLedger, TokenTotals},
    provider::{ProviderAuthRequest, ProviderPicker, ProviderPickerResult},
    renderer::{
        AnimationView, AssistantPhase, Block, BlockKind, ComposerMode, EffortSlider,
        HIDDEN_STATUS_LINE, IntegrationItemState, IntegrationItemView, LiveBlockView, ModeAccent,
        OverlayLine, OverlayStyle, OverlayView, PICKER_ROWS, PermissionBadge, PermissionTone,
        PlanStep, PlanStepStatus, PlanSummary, ProviderHandoffBlock, ProviderIntegrationView,
        SIDE_PANEL_WIDTHS, StatusLineView, SubagentView, SuggestionView, VibeTone, View,
        WelcomeView, visible_window,
    },
    rollout::{PlanSnapshot, Rollout, RolloutEvent, RolloutKind},
    theme::{self, ThemeKind},
};

const SPINNER: [&str; 8] = ["✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳"];

/// How long one shimmer sweep across the `Working` label takes.
const SHIMMER_PERIOD: Duration = Duration::from_millis(1_100);
/// Compaction is a separate wait state, so its activity animation advances at a
/// calmer pace than the ordinary response shimmer.
const COMPACTION_ACTIVITY_PERIOD: Duration = Duration::from_secs(2);
const PLAN_SHIMMER_DURATION: Duration = SHIMMER_PERIOD.saturating_mul(5);
const RESPONSE_COLLAPSE_DURATION: Duration = Duration::from_millis(120);

/// One-off notices (copy, reroute, …) sit in the status line this long.
const NOTICE_TTL: Duration = Duration::from_millis(1_400);
/// A second Ctrl+C only quits while its warning is still on screen, so the
/// armed state and the notice share one window.
const QUIT_ARM_WINDOW: Duration = Duration::from_secs(3);
/// How quiet a turn has to go before the runtime is asked whether it is still
/// running. Long enough that an ordinary think never triggers it.
const TURN_STALL_SILENCE: Duration = Duration::from_secs(20);

/// The permission presets Codex exposes through `/permissions`, cycled with Shift+Tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PermissionMode {
    FullAccess,
}

/// Claude Code's own permission modes. Every turn carries the choice to the
/// bridge, while `/permissions`, Shift+Tab, and the composer badge change it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum ClaudePermissionMode {
    #[default]
    Default,
    AcceptEdits,
    Plan,
    Auto,
    DontAsk,
    BypassPermissions,
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
}

/// Whether completed turns keep every progress response visible or fold the
/// progress above the final answer into its prompt disclosure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResponseDisplayMode {
    All,
    #[default]
    Completed,
}

impl ResponseDisplayMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Completed => "Completed",
        }
    }

    pub const fn config_value(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Completed => "completed",
        }
    }

    fn from_config_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => Some(Self::All),
            "completed" => Some(Self::Completed),
            _ => None,
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

/// Alt+P steps the docked side panel through three widths before it closes
/// again, rather than a plain on/off toggle — one press to open at the
/// narrowest width, repeat presses to widen, one more to close.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidePanelStage {
    #[default]
    Closed,
    Small,
    Medium,
    Large,
}

impl SidePanelStage {
    const CHOICES: [Self; 4] = [Self::Closed, Self::Small, Self::Medium, Self::Large];

    fn width(self) -> Option<usize> {
        match self {
            Self::Closed => None,
            Self::Small => Some(SIDE_PANEL_WIDTHS[0]),
            Self::Medium => Some(SIDE_PANEL_WIDTHS[1]),
            Self::Large => Some(SIDE_PANEL_WIDTHS[2]),
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Closed => Self::Small,
            Self::Small => Self::Medium,
            Self::Medium => Self::Large,
            Self::Large => Self::Closed,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Closed => "Off",
            Self::Small => "Small",
            Self::Medium => "Medium",
            Self::Large => "Large",
        }
    }

    fn index(self) -> usize {
        Self::CHOICES
            .iter()
            .position(|stage| *stage == self)
            .unwrap_or_default()
    }

    fn from_config_value(value: &str) -> Self {
        match value
            .trim()
            .trim_matches(['"', '\''])
            .to_ascii_lowercase()
            .as_str()
        {
            "small" => Self::Small,
            "medium" => Self::Medium,
            "large" => Self::Large,
            _ => Self::Closed,
        }
    }

    pub const fn config_value(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }
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

/// Repeated at the end of every preset notice. The rules say this too, but a
/// rule in the system prompt loses to the model's own habit of labelling its
/// next tool call in English — `Now the arming API, gap, and reset:` — so the
/// turn restates it where it is hardest to miss. `concat!` takes literals only,
/// so the sentence is spelled out in each arm rather than shared as a constant.
macro_rules! language_notice {
    () => {
        "진행 안내와 답변은 모두 한국어로 쓴다. \
         사용자에게 보이는 모든 text는 첫 글자가 한글 음절이어야 하고, \
         영어로 시작하는 진행 문장이나 도구 호출 앞 라벨은 쓰지 않는다. \
         영어 낱말 뒤에 한국어를 이어 붙이지도 않는다. \
         `Confirmed ... works.`, `Good, that closes correctly.`처럼 도구 결과에 대한 판정을 \
         영어 문장으로 적고 한국어를 잇지도 않는다. 확인 결과도 `확인했습니다.`처럼 한국어로 쓴다. \
         기술 식별자를 뺀 모든 낱말이 한국어여야 한다."
    };
}

/// The length caps are what make the presets useful, and they are also what
/// truncated the one answer that must stay whole — the one asking the user to
/// pick. A cap the turn repeats beats a rule it does not, so the exception is
/// restated beside the cap instead of only in the system prompt.
macro_rules! choice_notice {
    () => {
        "사용자에게 선택이나 승인을 요청할 때는 이 분량 제한을 적용하지 않는다. \
         AskUserQuestion 도구를 쓸 수 있으면 본문에 나열하지 말고 그 도구로 묻는다. \
         쓸 수 없을 때만 선택지와 각각의 결과를 빠뜨리지 않고 적고, \
         분량을 맞추려고 선택지를 줄이거나 문장을 도중에 끊지 않는다."
    };
}

impl VibeMode {
    const PICKER_CHOICES: [Self; 3] = [Self::Normal, Self::Vibe, Self::SuperVibe];

    pub const fn config_value(self) -> &'static str {
        match self {
            Self::Vibe => "vibe",
            Self::SuperVibe => "super_vibe",
            Self::Normal => "normal",
        }
    }
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

    const fn picker_index(self) -> usize {
        match self {
            Self::Normal => 0,
            Self::Vibe => 1,
            Self::SuperVibe => 2,
        }
    }

    const fn picker_label(self) -> &'static str {
        match self {
            Self::Normal => "Off",
            Self::Vibe => "On",
            Self::SuperVibe => "Super Vibe",
        }
    }

    const fn picker_detail(self) -> &'static str {
        match self {
            Self::Normal => "Diff와 명령어를 모두 표시합니다.",
            Self::Vibe => "Diff와 명령어를 압축해서 표시합니다.",
            Self::SuperVibe => "Diff와 명령어 등을 모두 숨깁니다.",
        }
    }

    /// What the turn tells the model about the preset it is answering under. The
    /// preset governs what the transcript collapses, not how the answer is
    /// written: one length rule now holds in every mode, and asking the answer to
    /// hide paths and identifiers on top of that only cost the user the one place
    /// they were still readable. What stays here is what a turn cannot get
    /// elsewhere — the choice exception and the language rule.
    pub const fn turn_notice(self) -> &'static str {
        match self {
            Self::Vibe => concat!(
                "현재 응답 모드: Vibe. ",
                choice_notice!(),
                " ",
                language_notice!(),
            ),
            Self::SuperVibe => concat!(
                "현재 응답 모드: Super Vibe. ",
                choice_notice!(),
                " ",
                language_notice!(),
            ),
            Self::Normal => concat!(
                "현재 응답 모드: Off. ",
                choice_notice!(),
                " ",
                language_notice!()
            ),
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

impl ClaudePermissionMode {
    const BASE_CHOICES: [Self; 3] = [Self::Default, Self::AcceptEdits, Self::Plan];
    const AUTO_CHOICES: [Self; 4] = [Self::Default, Self::AcceptEdits, Self::Plan, Self::Auto];
    const BYPASS_CHOICES: [Self; 4] = [
        Self::Default,
        Self::AcceptEdits,
        Self::Plan,
        Self::BypassPermissions,
    ];
    const ALL_CHOICES: [Self; 5] = [
        Self::Default,
        Self::AcceptEdits,
        Self::Plan,
        Self::BypassPermissions,
        Self::Auto,
    ];

    /// Indicator text, wording and symbols taken from the CLI's own mode line so
    /// the badge is recognisable to anyone who has used Claude Code.
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "ask permissions",
            Self::AcceptEdits => "⏵⏵ accept edits on",
            Self::Plan => "⏸ plan mode",
            Self::Auto => "⏵⏵ auto mode",
            Self::DontAsk => "don't ask",
            Self::BypassPermissions => "⏵⏵ bypass permissions",
        }
    }

    fn picker_label(self) -> &'static str {
        match self {
            Self::Default => "Ask permissions",
            Self::AcceptEdits => "Auto accept edits",
            Self::Plan => "Plan mode",
            Self::Auto => "Auto mode",
            Self::DontAsk => "Don't ask",
            Self::BypassPermissions => "Bypass permissions",
        }
    }

    /// The value the Claude Agent SDK takes for `permissionMode`.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    fn tone(self) -> PermissionTone {
        match self {
            Self::Default => PermissionTone::Neutral,
            Self::AcceptEdits => PermissionTone::AcceptEdits,
            Self::Plan => PermissionTone::Plan,
            Self::Auto => PermissionTone::Auto,
            Self::DontAsk => PermissionTone::Neutral,
            Self::BypassPermissions => PermissionTone::Bypass,
        }
    }

    /// Reads the wire value reported by Claude settings or a live session.
    pub(crate) fn from_wire(value: &str) -> Option<Self> {
        [
            Self::Default,
            Self::AcceptEdits,
            Self::Plan,
            Self::Auto,
            Self::DontAsk,
            Self::BypassPermissions,
        ]
        .into_iter()
        .find(|mode| mode.wire().eq_ignore_ascii_case(value))
    }

    fn choices(auto_available: bool, bypass_available: bool) -> &'static [Self] {
        match (auto_available, bypass_available) {
            (false, false) => &Self::BASE_CHOICES,
            (true, false) => &Self::AUTO_CHOICES,
            (false, true) => &Self::BYPASS_CHOICES,
            (true, true) => &Self::ALL_CHOICES,
        }
    }

    fn picker_index(self, auto_available: bool, bypass_available: bool) -> usize {
        Self::choices(auto_available, bypass_available)
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0)
    }

    fn next(self, auto_available: bool, bypass_available: bool) -> Self {
        let choices = Self::choices(auto_available, bypass_available);
        let current = choices
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(choices.len() - 1);
        choices[(current + 1) % choices.len()]
    }
}

/// Rows of the `/provider` picker, in the order they are drawn.
const RUNTIME_CHOICES: [&str; 2] = ["Claude", "Codex"];
const CLAUDE_PERMISSION_TABS: [&str; 5] =
    ["Allow", "Ask", "Deny", "Directories", "Recently denied"];
const CLAUDE_PERMISSION_SCOPES: [(&str, &str); 3] = [
    ("User settings", "user"),
    ("Project settings", "project"),
    ("Local settings", "local"),
];

fn claude_permission_behavior(tab: usize) -> Option<&'static str> {
    match tab {
        0 => Some("allow"),
        1 => Some("ask"),
        2 => Some("deny"),
        3 => Some("directory"),
        _ => None,
    }
}

fn claude_permission_source_label(source: &str) -> &str {
    match source {
        "user" | "userSettings" => "User settings",
        "project" | "projectSettings" => "Project settings",
        "local" | "localSettings" => "Local settings",
        "managed" => "Managed settings",
        "flag" => "Session settings",
        _ => source,
    }
}

/// Vibe settings keys holding which runtimes dvz may connect to. Absent means
/// not connected: the first launch on a machine connects nothing until the user
/// picks in `/provider`.
pub(crate) const CLAUDE_PROVIDER_KEY: &str = "claude_provider_enabled";
pub(crate) const CODEX_PROVIDER_KEY: &str = "codex_provider_enabled";

struct SlashCommand {
    name: &'static str,
    description: &'static str,
    takes_argument: bool,
}

const SLASH_COMMANDS: [SlashCommand; 32] = [
    SlashCommand {
        name: "/provider",
        description: "Switch between the Claude and Codex providers",
        takes_argument: true,
    },
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
        name: "/Response",
        description: "Set Response compression type",
        takes_argument: true,
    },
    SlashCommand {
        name: "/effort",
        description: "Set reasoning effort",
        takes_argument: true,
    },
    SlashCommand {
        name: "/permissions",
        description: "Manage the current provider's permission rules",
        takes_argument: false,
    },
    SlashCommand {
        name: "/theme",
        description: "Switch Minimal, Soft, or Dark theme",
        takes_argument: true,
    },
    SlashCommand {
        name: "/login",
        description: "Sign in to the current provider account",
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
        name: "/connect",
        description: "Connect an OpenCode provider",
        takes_argument: false,
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
        description: "Customize response, shell, and diff display (Alt+V cycles the preset)",
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
        name: "/side-panel",
        description: "Choose the docked side panel size",
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
    /// Provider-native usage rows, used by Claude subscriptions instead of reset credits.
    pub usage_lines: Vec<String>,
    pub five_hour_percent: Option<u8>,
    pub weekly_percent: Option<u8>,
    /// When the 5h window rolls over, as a Unix timestamp; `None` when the
    /// provider reported no reset time.
    pub five_hour_reset_at: Option<u64>,
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
            usage_lines: Vec::new(),
            five_hour_percent: None,
            weekly_percent: None,
            five_hour_reset_at: None,
        }
    }

    pub fn from_claude(account: Option<&Value>, usage: Option<&Value>) -> Self {
        let subscription = usage
            .and_then(|usage| usage.get("subscription_type"))
            .or_else(|| account.and_then(|account| account.get("subscriptionType")))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let window =
            |name: &str| usage.and_then(|usage| usage.pointer(&format!("/rate_limits/{name}")));
        let percent = |value: Option<&Value>| {
            value
                .and_then(|value| value.get("utilization"))
                .and_then(Value::as_f64)
                .map(|value| value.clamp(0.0, 100.0).round() as u8)
        };
        let five_hour = window("five_hour");
        let seven_day = window("seven_day");
        let mut usage_lines = Vec::new();
        for (label, value) in [("5h", five_hour), ("7d", seven_day)] {
            let Some(percent) = percent(value) else {
                continue;
            };
            let reset = value
                .and_then(|value| value.get("resets_at"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| format!(" · reset {}", compact_reset_time(value)))
                .unwrap_or_default();
            usage_lines.push(format!("{label} {percent}% used{reset}"));
        }
        Self {
            plan: subscription.map(|value| format!("Claude {}", title_case(value))),
            credits: Vec::new(),
            available_credits: 0,
            usage_lines,
            five_hour_percent: percent(five_hour),
            weekly_percent: percent(seven_day),
            five_hour_reset_at: five_hour
                .and_then(|value| value.get("resets_at"))
                .and_then(reset_timestamp),
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
        if !self.usage_lines.is_empty() {
            return self.usage_lines.clone();
        }
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

/// Reset times arrive either as an RFC 3339 string (Claude) or as a Unix
/// timestamp (Codex), so both shapes are accepted here.
fn reset_timestamp(value: &Value) -> Option<u64> {
    if let Some(seconds) = value.as_u64() {
        return Some(seconds);
    }
    let raw = value.as_str().filter(|raw| !raw.is_empty())?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .and_then(|instant| u64::try_from(instant.timestamp()).ok())
}

/// `3h 33m` style countdown to `reset_at`, or `None` once the window has
/// already rolled over or was never reported.
fn remaining_label(reset_at: Option<u64>, now: u64) -> Option<String> {
    let remaining = reset_at?.checked_sub(now).filter(|left| *left > 0)?;
    let minutes = remaining.div_ceil(60);
    let (hours, minutes) = (minutes / 60, minutes % 60);
    Some(if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{minutes}m")
    })
}

fn compact_reset_time(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|instant| {
            instant
                .with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_owned())
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

fn context_token_label(tokens: u64) -> String {
    format!("{}k", tokens / 1_000)
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
    pub supports_auto_mode: bool,
}

#[derive(Clone)]
pub struct EffortInfo {
    pub id: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelProvider {
    Codex,
    Claude,
    OpenCode,
}

impl ModelProvider {
    fn from_model(model: &str) -> Self {
        if model.starts_with("claude:") {
            Self::Claude
        } else if model.starts_with("opencode:") {
            Self::OpenCode
        } else {
            Self::Codex
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::OpenCode => "OpenCode",
        }
    }

    fn matches(self, model: &ModelInfo) -> bool {
        Self::from_model(&model.model) == self
    }
}

fn integration_item_order(state: IntegrationItemState) -> u8 {
    match state {
        IntegrationItemState::Inactive => 0,
        IntegrationItemState::Pending => 1,
        IntegrationItemState::Unknown => 2,
        IntegrationItemState::Active => 3,
    }
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
        let display_name = normalized_model_display_name(value.get("displayName")?.as_str()?);
        Some(Self {
            id: value.get("id")?.as_str()?.to_owned(),
            model: value.get("model")?.as_str()?.to_owned(),
            display_name,
            default_effort: value.get("defaultReasoningEffort")?.as_str()?.to_owned(),
            efforts,
            is_default: value
                .get("isDefault")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            context_window: value.get("contextWindow").and_then(Value::as_u64),
            supports_auto_mode: value
                .get("supportsAutoMode")
                .and_then(Value::as_bool)
                .unwrap_or(false),
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
            || self
                .model
                .strip_prefix("claude:")
                .is_some_and(|model| model.eq_ignore_ascii_case(query))
            || self.display_name.eq_ignore_ascii_case(query)
        {
            return true;
        }

        let query = query.trim().to_ascii_lowercase();
        let identity = format!("{} {}", self.model, self.display_name).to_ascii_lowercase();
        match query.as_str() {
            "claude" | "claude:default" => identity.contains("claude"),
            "haiku" => identity.contains("claude") && identity.contains("haiku"),
            "sonnet" => identity.contains("claude") && identity.contains("sonnet"),
            "opus" => identity.contains("claude") && identity.contains("opus"),
            "fable" => identity.contains("claude") && identity.contains("fable"),
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

fn normalized_model_display_name(display_name: &str) -> String {
    let Some(rest) = display_name.strip_prefix("GPT-") else {
        return display_name.to_owned();
    };
    let Some((version, variants)) = rest.split_once('-') else {
        return display_name.to_owned();
    };
    if version
        .chars()
        .all(|character| character.is_ascii_digit() || character == '.')
        && variants.split('-').all(|variant| {
            !variant.is_empty()
                && variant
                    .chars()
                    .all(|character| character.is_ascii_alphabetic())
        })
    {
        format!("GPT-{version} {}", variants.replace('-', " "))
    } else {
        display_name.to_owned()
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
    ActivateCodex,
    SetFast(bool),
    /// Hand Claude the permission mode the badge just cycled to.
    SetClaudePermissionMode(ClaudePermissionMode),
    DisableClaudeAutoMode,
    OpenClaudePermissions(Option<String>),
    UpdateClaudePermission {
        action: &'static str,
        behavior: String,
        value: String,
        destination: String,
    },
    RetryClaudePermissionDenial {
        tool: String,
        input: Value,
    },
    StartSide(Option<String>),
    ReturnFromSide,
    Compact,
    ScrollToBottom,
    ScrollToPrompt(u64),
    Copy(String),
    /// Fetch MCP server status and open the picker. Any notice is carried over
    /// so the result of the action that reopened it stays on screen.
    OpenMcp(Option<String>),
    McpLogin(String),
    /// Re-read the MCP configuration and restart the servers.
    ReconnectMcp(Option<String>),
    SetMcpEnabled {
        provider: SkillProvider,
        name: String,
        enabled: bool,
    },
    ConnectProvider,
    SubmitProviderAuth(Box<ProviderAuthRequest>),
    CompleteProviderOAuth {
        provider_id: String,
        provider_name: String,
        method: usize,
        code: String,
    },
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
    OpenSkills {
        provider: SkillProvider,
        notice: Option<String>,
    },
    SetSkill {
        provider: SkillProvider,
        name: String,
        enabled: bool,
    },
    /// Toggle the exact picker row without listing and resolving it again.
    SetSkillEnabled {
        provider: SkillProvider,
        name: String,
        path: String,
        source: Option<String>,
        scope: String,
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
    /// Save the picked side-panel size as the fallback for new sessions.
    PersistSidePanelDefault(SidePanelStage),
    /// Save whether completed progress responses stay visible or fold away.
    PersistResponseDisplayMode(ResponseDisplayMode),
    /// Save the transcript's Shell display preference for future sessions.
    PersistShellDisplayMode(ShellDisplayMode),
    PersistDiffDisplayMode(DiffDisplayMode),
    PersistVibeDisplayModes {
        vibe: VibeMode,
        response: ResponseLength,
        shell: ShellDisplayMode,
        diff: DiffDisplayMode,
    },
    PersistStatusLine {
        key_path: &'static str,
        enabled: bool,
    },
    /// Save which runtimes this machine may connect to. `activate_codex` rides
    /// along when the same keystroke also moved the session to Codex, so the
    /// app-server starts only after the connection is on disk.
    PersistProviderConnection {
        key_path: &'static str,
        connected: bool,
        activate_codex: bool,
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
    revision: u64,
    pace: TextPace,
}

/// Providers deliver assistant text at their own uneven cadence: a phrase in one
/// event, a single character in the next. Holding the arrivals here and revealing
/// a measured share of them keeps the visible pace steady.
///
/// The share is measured against elapsed time rather than against frames. A frame
/// that takes twice as long to paint would otherwise reveal the same amount of
/// text over twice the time, and a frame the runtime skipped would reveal nothing
/// at all — that is the same jitter this is meant to hide, just moved one layer
/// down.
/// How long the text already in hand should take to appear. Held deliberately
/// above the gap between bursts: the reveal runs a fraction of a second behind
/// what has arrived, and that cushion is what lets the pace stay even across a
/// burst instead of emptying and waiting.
const STREAM_TARGET_LATENCY: f32 = 0.45;
/// Clusters per second. The floor keeps a thin trickle moving; the ceiling keeps
/// a burst from arriving as one visible jump.
const STREAM_MIN_RATE: f32 = 25.0;
const STREAM_MAX_RATE: f32 = 1600.0;
/// How quickly the rate closes on the backlog's demand, per second. Both are
/// gentle on purpose. Assistant text arrives near fifty characters a second, so
/// the rate that matters is the average one, and a rate that chased each burst
/// would spend the answer alternating between a sprint and a wait.
const STREAM_RATE_ATTACK: f32 = 4.0;
const STREAM_RATE_DECAY: f32 = 1.5;
/// A stall — a slow repaint, a descheduled loop — must not turn into one large
/// reveal once the loop comes back.
const STREAM_MAX_STEP: Duration = Duration::from_millis(40);

/// How many characters at the end of the streamed text are still rising toward
/// full strength, and how fast that tail retreats. A longer tail spreads the rise
/// over more steps, so each one is a smaller change in brightness; the ceiling is
/// what keeps a whole phrase from reading as dim.
const STREAM_FADE_MAX_TAIL: f32 = 14.0;
/// Characters per second the tail gives back. Just under the rate text arrives
/// at, so the tail keeps its length while an answer flows and takes a moment to
/// clear once it ends instead of snapping to full strength.
const STREAM_FADE_SPEED: f32 = 40.0;
/// How long a finishing notice may wait for the text ahead of it. A provider can
/// hand over a long tail at once, and a turn that looks stuck is worse than a
/// last line that lands whole.
const HELD_NOTIFICATION_LIMIT: Duration = Duration::from_millis(1500);
/// Keep the fully revealed live answer on screen for at least one terminal paint
/// before its completion moves the same text into transcript history. The main
/// loop runs this pass every 4 ms; five quiet passes clear a 60 Hz paint boundary.
const FINAL_STREAM_FRAME_TICKS: u8 = 5;

/// What one reveal pass put on screen, and what it left waiting.
#[derive(Default)]
pub struct StreamReveal {
    pub clusters: usize,
    pub backlog: usize,
    /// The settling tail changed length, so the frame needs repainting even when
    /// no new character appeared.
    pub fade_changed: bool,
    /// Notices held behind the reveal were delivered on this pass.
    pub released: bool,
    /// A forced final stream frame was prepared before completion is delivered.
    pub final_frame_ready: bool,
}

impl StreamReveal {
    pub fn changed(&self) -> bool {
        self.clusters > 0 || self.fade_changed || self.released || self.final_frame_ready
    }
}

#[derive(Default)]
struct TextPace {
    pending: String,
    rate: f32,
    carry: f32,
}

impl TextPace {
    fn push(&mut self, delta: &str) {
        self.pending.push_str(delta);
    }

    fn take(&mut self, elapsed: Duration) -> Option<String> {
        if self.pending.is_empty() {
            // The rate is kept, not cleared. Claude's deltas arrive in bursts
            // separated by short gaps, and restarting from the floor at every gap
            // is what made a steady answer read as stop-and-go.
            self.carry = 0.0;
            return None;
        }
        let step = elapsed.min(STREAM_MAX_STEP).as_secs_f32();
        let backlog = visible_cluster_count(&self.pending) as f32;
        let demand = (backlog / STREAM_TARGET_LATENCY).clamp(STREAM_MIN_RATE, STREAM_MAX_RATE);
        let closing = if demand > self.rate {
            STREAM_RATE_ATTACK
        } else {
            STREAM_RATE_DECAY
        };
        self.rate += (demand - self.rate) * (closing * step).min(1.0);
        // Fractional budgets only stay even if the leftover carries to the next
        // reveal; truncating every time would quantize the pace to integers.
        let budget = self.rate * step + self.carry;
        let size = budget.floor();
        self.carry = budget - size;
        if size < 1.0 {
            return None;
        }
        let end = visible_cluster_end(&self.pending, size as usize);
        Some(self.pending.drain(..end).collect())
    }

    fn flush(&mut self) -> Option<String> {
        self.rate = 0.0;
        self.carry = 0.0;
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }
}

/// A joiner, variation selector, combining mark, or skin-tone modifier belongs to
/// the character before it. Splitting between them would paint a broken glyph for
/// one frame.
fn joins_previous(ch: char) -> bool {
    matches!(
        u32::from(ch),
        0x200d | 0x0300..=0x036f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0x20d0..=0x20f0
            | 0xfe00..=0xfe0f | 0x1f3fb..=0x1f3ff
    )
}

fn visible_cluster_count(text: &str) -> usize {
    let mut count = 0;
    let mut prev_joiner = false;
    for (index, ch) in text.char_indices() {
        if index == 0 || !(prev_joiner || joins_previous(ch)) {
            count += 1;
        }
        prev_joiner = ch == '\u{200d}';
    }
    count
}

/// Byte offset just past `clusters` visible characters.
fn visible_cluster_end(text: &str, clusters: usize) -> usize {
    let mut taken = 0;
    let mut prev_joiner = false;
    for (index, ch) in text.char_indices() {
        if index == 0 || !(prev_joiner || joins_previous(ch)) {
            if taken == clusters {
                return index;
            }
            taken += 1;
        }
        prev_joiner = ch == '\u{200d}';
    }
    text.len()
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

#[derive(Clone)]
struct ClaudePermissionEntry {
    behavior: String,
    value: String,
    source: String,
    mutable: bool,
}

#[derive(Clone)]
struct ClaudePermissionDenial {
    tool: String,
    reason: String,
    input: Value,
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
    SidePanelPicker {
        stage_index: usize,
    },
    SidePanelScope {
        stage: SidePanelStage,
        selected: usize,
    },
    /// `/provider`: which runtime the next prompt goes to, and which runtimes
    /// dvz may dial at all. A machine that cannot reach the Codex app-server
    /// switches Codex off here, and nothing calls it again until it is back on.
    RuntimePicker {
        selected: usize,
    },
    /// A subagent's recorded work, opened from its row under the composer. Read
    /// only: closing it hands the main thread straight back.
    SubagentTranscript {
        id: String,
        /// First visible line, so a long record can be walked with Up/Down.
        offset: usize,
    },
    SettingPicker {
        setting: DisplaySetting,
        selected: usize,
    },
    ClaudePermissionPicker {
        selected: usize,
    },
    ClaudeAutoModeConsent {
        selected: usize,
    },
    ClaudePermissionsPanel {
        tab: usize,
        selected: usize,
        entries: Vec<ClaudePermissionEntry>,
        denials: Vec<ClaudePermissionDenial>,
        retry: Option<usize>,
        rules_locked: bool,
    },
    ClaudePermissionScopePicker {
        behavior: String,
        selected: usize,
    },
    ClaudePermissionRuleInput {
        behavior: String,
        destination: String,
        editor: Editor,
    },
    VibeModePicker {
        selected: usize,
        vibe: VibeMode,
        response: ResponseLength,
        shell: ShellDisplayMode,
        diff: DiffDisplayMode,
    },
    StatusLinePicker {
        selected: usize,
    },
    SkillsPicker {
        provider: SkillProvider,
        selected: usize,
        skills: Vec<SkillBinding>,
        errors: Vec<String>,
        notice: Option<String>,
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
    ProviderLoading,
    ProviderPicker(ProviderPicker),
    ProviderOAuthCode {
        provider_id: String,
        provider_name: String,
        method: usize,
        url: String,
        instructions: String,
        editor: Editor,
        validation: Option<String>,
    },
    ProviderOAuthWaiting {
        provider_name: String,
        url: String,
        instructions: String,
    },
    Approval {
        id: Value,
        title: String,
        detail: Vec<String>,
        selected: usize,
        once: Value,
        session: Option<Value>,
        session_label: String,
        decline: Value,
    },
    UserInput {
        id: Value,
        questions: Vec<Question>,
        current: usize,
        selected: usize,
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
    Response,
    Shell,
    Diff,
    Fast,
}

impl DisplaySetting {
    fn title(self) -> &'static str {
        match self {
            Self::Response => "Response",
            Self::Shell => "Shell",
            Self::Diff => "Diff",
            Self::Fast => "Fast",
        }
    }

    fn choices(self) -> &'static [&'static str] {
        match self {
            Self::Response => &["All", "Completed"],
            Self::Shell | Self::Diff => &["Hide", "Collapse", "Expand"],
            Self::Fast => &["On", "Off"],
        }
    }

    fn detail(self, selected: usize) -> Option<String> {
        match (self, selected) {
            (Self::Response, 0) => Some(
                "Super Vibe 모드에서만 동작합니다. 모든 진행 응답을 항상 표시합니다."
                    .to_owned(),
            ),
            (Self::Response, 1) => Some(
                "Super Vibe 모드에서만 동작합니다. 완료되면 마지막 답변만 남기고 이전 응답을 접습니다."
                    .to_owned(),
            ),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillProvider {
    Claude,
    Codex,
}

impl SkillProvider {
    pub fn from_model(model: &str) -> Self {
        if model.starts_with("claude:") || model.starts_with("opencode:") {
            Self::Claude
        } else {
            Self::Codex
        }
    }

    pub const fn model_hint(self) -> &'static str {
        match self {
            Self::Claude => "claude:sonnet",
            Self::Codex => "gpt-5",
        }
    }

    const fn other(self) -> Self {
        match self {
            Self::Claude => Self::Codex,
            Self::Codex => Self::Claude,
        }
    }
}

#[derive(Clone)]
struct SkillBinding {
    name: String,
    path: String,
    description: String,
    enabled: bool,
    scope: String,
    source: Option<String>,
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
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.options.len().saturating_sub(1));
                None
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
                KeyCode::Delete if ctrl => self.editor.delete_word_right(),
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
                    .or_else(|| {
                        skill
                            .get("interface")
                            .and_then(|interface| interface.get("shortDescription"))
                            .and_then(Value::as_str)
                    })
                    .or_else(|| skill.get("shortDescription").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned(),
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
            })
        })
        .collect()
}

fn parse_skill_errors(response: &Value) -> Vec<String> {
    response
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .flat_map(|entry| {
            entry
                .get("errors")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
        })
        .filter_map(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect()
}

fn compact_skill_column(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    if max_width <= 1 {
        return "…".to_owned();
    }
    let mut output = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width >= max_width {
            break;
        }
        output.push(ch);
        width += ch_width;
    }
    output.push('…');
    output
}

fn skill_picker_row(skill: &SkillBinding, name_width: usize) -> String {
    let label = compact_skill_column(
        &format!("[{}] {}", if skill.enabled { 'x' } else { ' ' }, skill.name),
        name_width,
    );
    let padding = " ".repeat(name_width.saturating_sub(UnicodeWidthStr::width(label.as_str())));
    let mut description = skill
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(source) = skill.source.as_deref() {
        if !description.is_empty() {
            description.push_str(" · ");
        }
        description.push_str(&format!("{source} plugin 전체 전환"));
    }
    if description.is_empty() {
        label
    } else {
        format!("{label}{padding}  {description}")
    }
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
pub enum ConversationView {
    #[default]
    List,
    Chat,
}

impl ConversationView {
    pub const fn is_chat(self) -> bool {
        matches!(self, Self::Chat)
    }
}

fn visible_resume_picker_rows() -> usize {
    let height = terminal::size().map(|(_, height)| height).unwrap_or(30);
    resume_picker_rows(height)
}

/// Rows the `Apply to` step prints before its choices: the pick it is confirming
/// and the blank under it.
const MODEL_SCOPE_HEADER_ROWS: usize = 2;
const SKILLS_PICKER_ROWS: usize = 10;
const SKILL_NAME_MAX_COLUMNS: usize = 32;

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
            | PendingInteraction::SidePanelPicker { .. }
            | PendingInteraction::SidePanelScope { .. }
            | PendingInteraction::RuntimePicker { .. }
            | PendingInteraction::SettingPicker { .. }
            | PendingInteraction::ClaudePermissionPicker { .. }
            | PendingInteraction::ClaudePermissionsPanel { .. }
            | PendingInteraction::ClaudePermissionScopePicker { .. }
            | PendingInteraction::ClaudePermissionRuleInput { .. }
            | PendingInteraction::VibeModePicker { .. }
            | PendingInteraction::StatusLinePicker { .. }
            | PendingInteraction::SkillsPicker { .. }
            | PendingInteraction::SubagentTranscript { .. }
            | PendingInteraction::SessionPicker(_)
            | PendingInteraction::ProviderPicker(_)
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
            KeyCode::Delete if ctrl => {
                self.query.delete_word_right();
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

fn submission_display(source: &str, image_count: usize) -> String {
    let mut display = String::new();
    let mut image_index = 0;
    let chars = source.chars().collect::<Vec<_>>();
    for (index, ch) in chars.iter().copied().enumerate() {
        if ch != ATTACHMENT_PLACEHOLDER {
            display.push(ch);
            continue;
        }
        image_index += 1;
        if image_index > image_count {
            continue;
        }
        if display.chars().last().is_some_and(|ch| !ch.is_whitespace()) {
            display.push(' ');
        }
        display.push_str(&format!("[Image #{image_index}]"));
        if chars
            .get(index + 1)
            .is_some_and(|&next| next != ATTACHMENT_PLACEHOLDER && !next.is_whitespace())
        {
            display.push(' ');
        }
    }
    display
}

pub struct TickResult {
    pub redraw: bool,
    pub animation_only: bool,
}

#[derive(Clone, Copy, Debug)]
struct ResponseCollapseTransition {
    group_id: u64,
    started_at: Instant,
}

/// One subagent the provider is currently running for this session. The bridge
/// reports elapsed time per update, but the row ticks between updates, so the
/// start instant is kept locally and reused while the same id stays running.
#[derive(Clone, Debug)]
struct RunningSubagent {
    id: String,
    name: String,
    description: String,
    tool: String,
    started_at: Instant,
    painted_elapsed_secs: u64,
}

/// One recorded line of a subagent's work, as shown in its transcript panel.
#[derive(Clone, Debug)]
struct SubagentLogLine {
    text: String,
    muted: bool,
}

/// A long-running subagent can emit far more than a panel will ever show, so the
/// oldest lines are dropped rather than held for the rest of the session.
const SUBAGENT_LOG_LIMIT: usize = 400;

#[derive(Clone)]
struct ProviderIntegrationSnapshot {
    mcp_expanded: bool,
    plugins_expanded: bool,
    mcp: Option<Vec<IntegrationItemView>>,
    plugins: Option<Vec<IntegrationItemView>>,
    mcp_error: Option<String>,
    plugin_error: Option<String>,
}

impl Default for ProviderIntegrationSnapshot {
    fn default() -> Self {
        Self {
            mcp_expanded: true,
            plugins_expanded: true,
            mcp: None,
            plugins: None,
            mcp_error: None,
            plugin_error: None,
        }
    }
}

#[derive(Clone)]
struct McpFailure {
    provider: ModelProvider,
    name: String,
    detail: Option<String>,
}

pub struct AppState {
    pub editor: Editor,
    composer_images: Vec<String>,
    queued_prompts: VecDeque<String>,
    pub thread_id: String,
    /// The id a later `-r` has to use for this thread, when that is no longer the
    /// thread's own id. A Claude-named room whose turns moved to Codex keeps its
    /// visible id on screen, but only the rollout id resumes the conversation.
    resume_id: String,
    pub turn_id: Option<String>,
    /// Set when the user interrupts after `turn/start` answers but before the
    /// app-server has announced that the turn is active.
    pending_interrupt: bool,
    turn_interrupted: bool,
    /// When this turn last showed a sign of life. A `turn/completed` that never
    /// arrives would otherwise leave the activity row waiting on nothing.
    turn_progress_at: Option<Instant>,
    /// When the last stall probe went out, so a quiet turn is asked about at a
    /// steady pace instead of once per frame.
    stall_probe_at: Option<Instant>,
    /// When the last Ctrl+C armed the quit. Stale arms expire with `QUIT_ARM_WINDOW`
    /// so a Ctrl+C pressed long after the warning faded never quits on its own.
    quit_armed_at: Option<Instant>,
    pub busy: bool,
    host_loading: bool,
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
    /// Foldable transcript milestones in their live order: assistant messages
    /// plus any context compaction that happened between them. Codex labels
    /// commentary and final output; runtimes without that label use the last
    /// message on a successful turn as the final answer.
    turn_response_blocks: Vec<Block>,
    /// User prompt block ids for the active turn, including every steer. A
    /// steer joins the existing provider turn and gets no response id of its
    /// own, so these local creation boundaries keep later progress attached to
    /// the prompt that preceded it.
    turn_prompt_ids: Vec<u64>,
    response_grouped: bool,
    response_collapse: Option<ResponseCollapseTransition>,
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
    plan_summary: Option<PlanSummary>,
    /// The turn that published the visible plan. A later turn must not revive
    /// its completed final step while it is still waiting for its own plan.
    plan_turn_id: Option<String>,
    plan_shimmer_started_at: Option<Instant>,
    subagents: Vec<RunningSubagent>,
    /// Recorded subagent work, keyed by the parent tool-use id. Kept for the rest
    /// of the turn so a panel opened on a finished subagent still has something
    /// to show.
    subagent_logs: HashMap<String, Vec<SubagentLogLine>>,
    command_selection: usize,
    spinner_frame: usize,
    turn_started_at: Option<Instant>,
    /// Whether the active turn has painted any assistant text yet.
    turn_response_started: bool,
    /// How many characters at the end of the streamed text are still settling.
    /// Each reveal lengthens it and time shortens it, so the tail is long while
    /// text is flowing and gone shortly after it stops.
    stream_fade_tail: f32,
    /// Notices waiting for the text still being revealed, in arrival order.
    held_notifications: Vec<(String, Value)>,
    held_since: Option<Instant>,
    /// Quiet reveal ticks left before a fully visible live answer may complete.
    held_final_frame_ticks: u8,
    /// When `/compact` was sent. Compaction produces no assistant text, so the
    /// activity row runs its own clock until the runtime reports the boundary.
    compacting_started_at: Option<Instant>,
    last_completed_duration: Option<Duration>,
    branch: Option<String>,
    five_hour_percent: Option<u8>,
    weekly_percent: Option<u8>,
    /// Unix timestamp the 5h window resets at, so the status row can count down.
    five_hour_reset_at: Option<u64>,
    /// The countdown as last painted (`3h 33m`), kept so the minute tick can
    /// trigger a redraw on its own.
    five_hour_remaining: Option<String>,
    fast_mode: bool,
    /// Claude's permission mode for this thread and the optional modes exposed
    /// by its own resolved settings.
    claude_permission_mode: ClaudePermissionMode,
    bypass_permissions_allowed: bool,
    claude_auto_mode_disabled: bool,
    claude_auto_mode_confirmed: bool,
    side_parent: Option<SideParent>,
    last_assistant_markdown: Option<String>,
    composer_notice: Option<(String, Instant)>,
    /// Text, when it went up, and how long it stays. The quit warning needs a
    /// longer window than the rest, so the lifetime rides along with the notice.
    activity_notice: Option<(String, Instant, Duration)>,
    status_metadata_refreshed_at: Instant,
    response_length: ResponseLength,
    response_display_mode: ResponseDisplayMode,
    vibe_mode: VibeMode,
    conversation_view: ConversationView,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    /// The docked right-hand side panel's width stage. Persisted across
    /// sessions so a panel left open reopens the same way next time.
    side_panel_stage: SidePanelStage,
    side_panel_prompts_expanded: bool,
    status_line_settings: StatusLineSettings,
    /// Which runtimes this machine may connect to. Both start off — a fresh
    /// install picks in `/provider` — and nothing dials a runtime that is off.
    claude_provider_enabled: bool,
    codex_provider_enabled: bool,
    /// Set at launch when this machine has never picked a runtime. While it is
    /// up the composer holds prompts back and points at the picker.
    provider_choice_pending: bool,
    account_plan: AccountPlan,
    /// Set when a login lands, so the event loop re-reads the account over RPC.
    account_refresh_due: bool,
    skills: Vec<SkillBinding>,
    mentions: Vec<MentionBinding>,
    app_mentions: Vec<MentionBinding>,
    workspace_entries: Vec<CompletionCandidate>,
    completion_catalog: Vec<CompletionCandidate>,
    completion_mode: CompletionMode,
    suggestions_dismissed_text: Option<String>,
    selected_completion_bindings: Vec<SelectedCompletionBinding>,
    claude_integrations: ProviderIntegrationSnapshot,
    codex_integrations: ProviderIntegrationSnapshot,
    /// MCP servers that failed to start, reported before any picker was open.
    mcp_failures: Vec<McpFailure>,
    /// A session chosen while `thread/start` was still in flight. `thread/resume`
    /// needs a bound thread to switch away from, so the target waits here until the
    /// event loop can run it.
    deferred_resume: Option<DeferredResume>,
    /// Mode changes made while the first session is still being created. Their
    /// visible state changes immediately; the thread/config RPCs run as soon as
    /// that session has an id.
    deferred_startup_actions: Vec<Action>,
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

/// Every provider's plan reaches the same state layer. Keep the visible step
/// titles sequential even when a provider omitted, reused, or styled its own
/// number differently.
fn numbered_plan_step(title: &str, index: usize) -> String {
    let title = title.trim();
    let digits = title.chars().take_while(char::is_ascii_digit).count();
    let body = title
        .get(digits..)
        .and_then(|rest| rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')))
        .map(str::trim_start)
        .filter(|rest| !rest.is_empty())
        .unwrap_or(title);
    let body = if body.is_empty() { "작업" } else { body };
    format!("{}. {body}", index + 1)
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
        let (default_response_length, default_shell_display_mode, default_diff_display_mode) =
            match vibe_mode {
                VibeMode::Vibe => (
                    ResponseLength::Short,
                    ShellDisplayMode::Collapse,
                    DiffDisplayMode::Collapse,
                ),
                VibeMode::SuperVibe => (
                    ResponseLength::Short,
                    ShellDisplayMode::Hide,
                    DiffDisplayMode::Hide,
                ),
                VibeMode::Normal => (
                    ResponseLength::Short,
                    ShellDisplayMode::Expand,
                    DiffDisplayMode::Expand,
                ),
            };
        let response_length = read_vibe_config_value("model_verbosity")
            .map(|value| match value.as_str() {
                "medium" => ResponseLength::Normal,
                "high" => ResponseLength::Detailed,
                _ => ResponseLength::Short,
            })
            .unwrap_or(default_response_length);
        let response_display_mode = read_vibe_config_value("response_display_mode")
            .and_then(|value| ResponseDisplayMode::from_config_value(&value))
            .unwrap_or_default();
        let shell_display_mode = read_vibe_config_value("shell_display_mode")
            .and_then(|value| ShellDisplayMode::from_config_value(&value))
            .unwrap_or(default_shell_display_mode);
        let diff_display_mode = read_vibe_config_value("diff_display_mode")
            .and_then(|value| DiffDisplayMode::from_config_value(&value))
            .unwrap_or(default_diff_display_mode);
        // A Claude launch has no codex-usage.json of its own; reading it here would
        // show the last Codex session's numbers under a Claude status row until the
        // real Claude usage arrives.
        let (five_hour_percent, weekly_percent, five_hour_reset_at) =
            if crate::claude::is_claude_model(model) {
                (None, None, None)
            } else {
                read_codex_usage()
            };
        let context_window = models
            .get(selected_model)
            .and_then(|model| model.context_window);
        let mut state = Self {
            editor: Editor::default(),
            composer_images: Vec::new(),
            queued_prompts: VecDeque::new(),
            thread_id,
            resume_id: String::new(),
            turn_id: None,
            pending_interrupt: false,
            turn_interrupted: false,
            turn_progress_at: None,
            stall_probe_at: None,
            quit_armed_at: None,
            busy: false,
            host_loading: false,
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
            turn_response_blocks: Vec::new(),
            turn_prompt_ids: Vec::new(),
            response_grouped: false,
            response_collapse: None,
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
            plan_summary: None,
            plan_turn_id: None,
            plan_shimmer_started_at: None,
            subagents: Vec::new(),
            subagent_logs: HashMap::new(),
            command_selection: 0,
            spinner_frame: 0,
            compacting_started_at: None,
            turn_started_at: None,
            turn_response_started: false,
            stream_fade_tail: 0.0,
            held_notifications: Vec::new(),
            held_since: None,
            held_final_frame_ticks: 0,
            last_completed_duration: None,
            branch,
            five_hour_percent,
            weekly_percent,
            five_hour_reset_at,
            five_hour_remaining: remaining_label(five_hour_reset_at, unix_now()),
            fast_mode: read_fast_mode(),
            claude_permission_mode: ClaudePermissionMode::Default,
            bypass_permissions_allowed: false,
            claude_auto_mode_disabled: false,
            claude_auto_mode_confirmed: false,
            side_parent: None,
            last_assistant_markdown: None,
            composer_notice: None,
            activity_notice: None,
            status_metadata_refreshed_at: Instant::now(),
            vibe_mode,
            conversation_view,
            response_length,
            response_display_mode,
            shell_display_mode,
            diff_display_mode,
            // The stage belongs to a session, and no session is bound yet. Starting
            // closed is what keeps a brand new session from flashing a panel open
            // before its own (empty) stage is restored.
            side_panel_stage: SidePanelStage::Closed,
            side_panel_prompts_expanded: true,
            status_line_settings: read_status_line_settings(),
            claude_provider_enabled: claude_provider_enabled(),
            codex_provider_enabled: codex_provider_enabled(),
            provider_choice_pending: false,
            account_plan: AccountPlan::default(),
            account_refresh_due: false,
            skills: Vec::new(),
            mentions: Vec::new(),
            app_mentions: Vec::new(),
            workspace_entries: Vec::new(),
            completion_catalog: Vec::new(),
            completion_mode: CompletionMode::All,
            suggestions_dismissed_text: None,
            selected_completion_bindings: Vec::new(),
            claude_integrations: ProviderIntegrationSnapshot::default(),
            codex_integrations: ProviderIntegrationSnapshot::default(),
            mcp_failures: Vec::new(),
            deferred_resume: None,
            deferred_startup_actions: Vec::new(),
        };
        if let Some(count) = env::var("DEVEZ_VIBE_TEST_PLAN_STEPS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|&count| count > 0)
        {
            state.plan_summary = Some(PlanSummary {
                explanation: Some("Shimmer 테스트".to_owned()),
                steps: (1..=count)
                    .map(|index| PlanStep {
                        text: format!("테스트 작업 {index}"),
                        status: PlanStepStatus::Pending,
                        started_at: None,
                        elapsed: None,
                    })
                    .collect(),
                expanded: true,
                started_at: Instant::now(),
                elapsed: None,
            });
            state.plan_shimmer_started_at = Some(Instant::now());
        }
        state
    }

    pub fn selected_model(&self) -> Option<&ModelInfo> {
        self.models.get(self.selected_model)
    }

    pub fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    fn selected_provider(&self) -> ModelProvider {
        ModelProvider::from_model(self.selected_model_name())
    }

    pub fn is_selected_provider_model(&self, model: &str) -> bool {
        self.selected_provider() == ModelProvider::from_model(model)
    }

    fn selected_integration_snapshot_mut(&mut self) -> Option<&mut ProviderIntegrationSnapshot> {
        self.integration_snapshot_mut(self.selected_provider())
    }

    fn integration_snapshot_mut(
        &mut self,
        provider: ModelProvider,
    ) -> Option<&mut ProviderIntegrationSnapshot> {
        match provider {
            ModelProvider::Claude => Some(&mut self.claude_integrations),
            ModelProvider::Codex => Some(&mut self.codex_integrations),
            ModelProvider::OpenCode => None,
        }
    }

    fn side_panel_integration_views(&self) -> Vec<ProviderIntegrationView> {
        let selected = self.selected_provider();
        [
            (
                "Claude",
                self.claude_provider_enabled,
                selected == ModelProvider::Claude,
                &self.claude_integrations,
            ),
            (
                "Codex",
                self.codex_provider_enabled,
                selected == ModelProvider::Codex,
                &self.codex_integrations,
            ),
        ]
        .into_iter()
        .map(
            |(provider, enabled, active, snapshot)| ProviderIntegrationView {
                provider: provider.to_owned(),
                enabled,
                active,
                mcp_expanded: snapshot.mcp_expanded,
                plugins_expanded: snapshot.plugins_expanded,
                mcp: snapshot.mcp.clone(),
                plugins: snapshot.plugins.clone(),
                mcp_error: snapshot.mcp_error.clone(),
                plugin_error: snapshot.plugin_error.clone(),
            },
        )
        .collect()
    }

    fn provider_switch_pending(&self) -> bool {
        self.busy
            && self
                .active_turn_model
                .as_deref()
                .or(self.pending_turn_model.as_deref())
                .is_some_and(|model| ModelProvider::from_model(model) != self.selected_provider())
    }

    fn provider_model_indices(&self, provider: ModelProvider) -> Vec<usize> {
        self.models
            .iter()
            .enumerate()
            .filter_map(|(index, model)| provider.matches(model).then_some(index))
            .collect()
    }

    fn current_provider_model_indices(&self) -> Vec<usize> {
        self.provider_model_indices(self.selected_provider())
    }

    /// Where the current provider sits in `RUNTIME_CHOICES`. OpenCode has no
    /// row of its own, so it reads as Claude — the runtime it runs under.
    fn runtime_choice_index(&self) -> usize {
        match self.selected_provider() {
            ModelProvider::Codex => 1,
            _ => 0,
        }
    }

    /// Whether the runtime on a `/provider` row may be dialled at all. Nothing is
    /// connected until the user says so, on this machine, in this picker.
    fn runtime_connected(&self, index: usize) -> bool {
        if index == 1 {
            self.codex_provider_enabled
        } else {
            self.claude_provider_enabled
        }
    }

    pub fn any_provider_connected(&self) -> bool {
        self.claude_provider_enabled || self.codex_provider_enabled
    }

    pub fn open_runtime_picker(&mut self) {
        self.pending = Some(PendingInteraction::RuntimePicker {
            selected: self.runtime_choice_index(),
        });
    }

    /// Called once before the first frame. A machine with no runtime chosen yet
    /// opens the picker instead of assuming one, which is the whole point of
    /// having no default: the PC that cannot reach Codex never calls it.
    pub fn prompt_for_provider_if_unconnected(&mut self) {
        if self.any_provider_connected() {
            return;
        }
        self.provider_choice_pending = true;
        self.push_notice(
            BlockKind::System,
            "Provider 선택",
            "사용할 provider를 선택하세요. Enter로 연결하고 전환합니다. (나중에 /provider)",
        );
        self.open_runtime_picker();
    }

    /// Enter on a `/provider` row: use that runtime. A row that is not connected
    /// yet connects first — choosing it *is* the connection — and the choice is
    /// saved, so the next launch starts where this one left off.
    fn apply_runtime_choice(&mut self, index: usize) -> Action {
        let connecting = !self.runtime_connected(index);
        let key_path = self.set_runtime_connection(index, true);
        let activate_codex = index == 1 && self.selected_provider() != ModelProvider::Codex;
        match index {
            1 if !activate_codex => self.switch_provider(ModelProvider::Codex),
            1 => {}
            _ => self.switch_provider(ModelProvider::Claude),
        }
        if connecting {
            Action::PersistProviderConnection {
                key_path,
                connected: true,
                activate_codex,
            }
        } else if activate_codex {
            Action::ActivateCodex
        } else {
            Action::None
        }
    }

    /// Space on a `/provider` row: connect or disconnect that runtime and record
    /// it for later launches. Dropping the runtime in use hands the session to
    /// whatever is still connected; if nothing is, the composer waits for a pick.
    fn toggle_runtime_connection(&mut self, index: usize) -> Action {
        let connected = !self.runtime_connected(index);
        let key_path = self.set_runtime_connection(index, connected);
        let mut activate_codex = false;
        if !connected && index == self.runtime_choice_index() {
            if index == 1 && self.claude_provider_enabled {
                self.switch_provider(ModelProvider::Claude);
            } else if index == 0 && self.codex_provider_enabled {
                activate_codex = true;
            }
        }
        // Dropping the last connection puts the session back where a fresh
        // install starts: nothing runs until something is picked.
        self.provider_choice_pending = !self.any_provider_connected();
        Action::PersistProviderConnection {
            key_path,
            connected,
            activate_codex,
        }
    }

    /// Flips one runtime's connection and answers with the settings key that
    /// records it, so the caller can hand the write to the event loop.
    fn set_runtime_connection(&mut self, index: usize, connected: bool) -> &'static str {
        self.provider_choice_pending = false;
        if index == 1 {
            self.codex_provider_enabled = connected;
            CODEX_PROVIDER_KEY
        } else {
            self.claude_provider_enabled = connected;
            CLAUDE_PROVIDER_KEY
        }
    }

    /// Put a connection back the way it was when the write that should have
    /// recorded it failed, so the picker never shows a state disk disagrees with.
    pub fn restore_provider_connection(&mut self, key_path: &str, connected: bool) {
        if key_path == CODEX_PROVIDER_KEY {
            self.codex_provider_enabled = connected;
        } else {
            self.claude_provider_enabled = connected;
        }
    }

    /// One `/provider` step states both facts independently: whether this
    /// runtime may connect and whether this session currently uses it.
    fn runtime_step_label(&self, index: usize) -> String {
        let connection = if self.runtime_connected(index) {
            "연결됨"
        } else {
            "연결 안 됨"
        };
        let usage = if index == self.runtime_choice_index() {
            "사용 중"
        } else {
            "미사용"
        };
        format!("{} · {connection} · {usage}", RUNTIME_CHOICES[index])
    }

    fn switch_provider(&mut self, provider: ModelProvider) {
        self.commit_welcome_card();
        if self.selected_provider() == provider {
            self.committed.push(Block::new(
                BlockKind::System,
                "Provider",
                format!("현재 {} provider를 사용 중입니다.", provider.label()),
            ));
            return;
        }

        let candidates = self.provider_model_indices(provider);
        let selected = candidates
            .iter()
            .copied()
            .find(|index| self.models[*index].is_default)
            .or_else(|| candidates.first().copied());
        let Some(index) = selected else {
            self.committed.push(Block::new(
                BlockKind::Error,
                "Provider unavailable",
                format!("{} 모델을 찾을 수 없습니다.", provider.label()),
            ));
            return;
        };

        let model = &self.models[index];
        let model_name = model.display_name.clone();
        let effort = model.default_effort.clone();
        self.selected_model = index;
        self.selected_effort = effort.clone();
        self.context_window = model.context_window;
        self.normalize_claude_permission_mode_for_selected_model();
        let detail = if effort.is_empty() {
            format!("↳ {} · {model_name}", provider.label())
        } else {
            format!("↳ {} · {model_name} · {effort}", provider.label())
        };
        self.committed.push(Block::new(
            BlockKind::ModelChange,
            "✓ Provider changed",
            detail,
        ));
        self.refresh_usage_for_selected_provider();
    }

    /// Repaints the 5h/week rows from the runtime that just took over. Without
    /// this the status row keeps the previous provider's numbers until the next
    /// three-second metadata tick, which reads them for the new provider anyway.
    fn refresh_usage_for_selected_provider(&mut self) {
        let (five_hour_percent, weekly_percent, five_hour_reset_at) =
            if self.selected_provider() == ModelProvider::Claude {
                (
                    self.account_plan.five_hour_percent,
                    self.account_plan.weekly_percent,
                    self.account_plan.five_hour_reset_at,
                )
            } else {
                read_codex_usage()
            };
        self.five_hour_percent = five_hour_percent;
        self.weekly_percent = weekly_percent;
        self.five_hour_reset_at = five_hour_reset_at;
        self.five_hour_remaining = remaining_label(five_hour_reset_at, unix_now());
    }

    pub fn switch_to_codex(&mut self) {
        self.switch_provider(ModelProvider::Codex);
    }

    pub fn fallback_from_codex(&mut self, message: impl Into<String>) -> bool {
        if self.selected_provider() != ModelProvider::Codex {
            return false;
        }
        if self
            .provider_model_indices(ModelProvider::Claude)
            .is_empty()
        {
            self.push_notice(
                BlockKind::Error,
                "Codex 사용 불가",
                format!("{}\nClaude 모델도 찾을 수 없습니다.", message.into()),
            );
            return false;
        }

        let message = message.into();
        if self.busy {
            self.set_request_failed(message.clone());
        }
        self.push_notice(
            BlockKind::Warning,
            "Codex 사용 불가",
            format!("{message}\nClaude provider로 자동 전환했습니다."),
        );
        self.switch_provider(ModelProvider::Claude);
        true
    }

    /// Binds the id the host has to resume from when it differs from the thread's own
    /// id. Called after a thread is attached, since the routing that knows about a
    /// past provider switch lives in the backend, not in the thread id.
    pub fn note_resume_id(&mut self, resume_id: &str) {
        self.resume_id = if resume_id == self.thread_id {
            String::new()
        } else {
            resume_id.to_owned()
        };
    }

    /// The session id the host records so its next launch can `-r` back into this
    /// conversation. It is the thread's own id until a rebind moves the conversation
    /// into another runtime's session.
    pub fn host_session_id(&self) -> &str {
        if self.resume_id.is_empty() {
            &self.thread_id
        } else {
            &self.resume_id
        }
    }

    pub fn replace_models(&mut self, models: Vec<ModelInfo>) {
        if models.is_empty() {
            return;
        }
        let selected = self.selected_model_name().to_owned();
        self.models = models;
        self.selected_model = self
            .models
            .iter()
            .position(|model| model.model == selected)
            .or_else(|| self.models.iter().position(|model| model.is_default))
            .unwrap_or(0);
        let model = &self.models[self.selected_model];
        if !model.supports_effort(&self.selected_effort) {
            self.selected_effort = model.default_effort.clone();
        }
        self.normalize_claude_permission_mode_for_selected_model();
    }

    /// True until `thread/start` answers. The UI is fully painted before that, so
    /// anything that would talk to the thread has to wait for it.
    pub fn thread_pending(&self) -> bool {
        self.thread_id.is_empty()
    }

    /// Keeps DevezCode's tab spinner visible while a resumed transcript is
    /// being rebuilt, without treating the composer as an active turn.
    pub fn set_host_loading(&mut self, loading: bool) {
        self.host_loading = loading;
    }

    /// Compaction counts as work for the host tab too: the session is unavailable
    /// for a prompt until it finishes, exactly like a turn.
    pub fn host_turn_busy(&self) -> bool {
        self.busy || self.compacting()
    }

    pub fn host_loading(&self) -> bool {
        self.host_loading
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

    /// Keeps only the latest value of each startup-safe setting. Replaying every
    /// intermediate click after the thread appears would briefly apply stale modes.
    pub fn defer_startup_action(&mut self, action: Action) {
        let same_setting = |pending: &Action| {
            matches!(
                (pending, &action),
                (Action::SetFast(_), Action::SetFast(_))
                    | (
                        Action::PersistResponseDisplayMode(_),
                        Action::PersistResponseDisplayMode(_)
                    )
                    | (
                        Action::SetClaudePermissionMode(_),
                        Action::SetClaudePermissionMode(_)
                    )
                    | (
                        Action::PersistVibeDisplayModes { .. },
                        Action::PersistVibeDisplayModes { .. }
                    )
            )
        };
        self.deferred_startup_actions
            .retain(|pending| !same_setting(pending));
        self.deferred_startup_actions.push(action);
    }

    pub fn take_deferred_startup_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.deferred_startup_actions)
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
        self.resume_id = String::new();
        let cwd = plain_folder(cwd);
        if self.cwd != cwd {
            self.cwd = cwd;
            self.branch = read_git_branch(&self.cwd);
            self.workspace_entries.clear();
            self.rebuild_completion_catalog();
        }
        self.restore_session_side_panel();
        self.restore_session_modes();
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

    /// Puts the session back into its pending state while the replacement thread
    /// loads, so the cleared screen is immediately repainted with loading status.
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
        if plan.five_hour_percent.is_some() {
            self.five_hour_percent = plan.five_hour_percent;
        }
        if plan.weekly_percent.is_some() {
            self.weekly_percent = plan.weekly_percent;
        }
        if plan.five_hour_reset_at.is_some() {
            self.five_hour_reset_at = plan.five_hour_reset_at;
            self.five_hour_remaining = remaining_label(self.five_hour_reset_at, unix_now());
        }
        self.account_plan = plan;
    }

    /// Picker keys must bypass the composer paste buffer so controls such as
    /// Space reach their pending interaction immediately.
    pub fn has_pending_interaction(&self) -> bool {
        self.pending.is_some()
    }

    /// The free-text row is an editor as soon as it owns focus, matching Claude
    /// Code's `Other` option. Its buffered keys carry this identity so they can
    /// never be redirected into the main composer or a later question.
    pub fn pending_text_input_target(&self) -> Option<String> {
        let PendingInteraction::UserInput {
            id,
            questions,
            current,
            selected,
            ..
        } = self.pending.as_ref()?
        else {
            return None;
        };
        let question = questions.get(*current)?;
        user_input_text_focused(question, *selected)
            .then(|| format!("{}\u{1f}{current}\u{1f}{}", id, question.id))
    }

    /// Free-text question answers need the same short input delay as the main
    /// composer. Windows Terminal clears its IME preedit just after delivering
    /// the committed Hangul character; repainting before that clear erases the
    /// character from the screen even though it reached the editor.
    pub fn buffers_pending_text_input(&self) -> bool {
        self.pending_text_input_target().is_some()
    }

    pub fn update_skills(&mut self, response: &Value) {
        self.skills = parse_skill_bindings(response);
        self.rebuild_completion_catalog();
    }

    pub fn update_skills_for_provider(&mut self, provider: SkillProvider, response: &Value) {
        if SkillProvider::from_model(self.selected_model_name()) == provider {
            self.update_skills(response);
        }
    }

    pub fn apply_skill_enabled(
        &mut self,
        provider: SkillProvider,
        path: &str,
        source: Option<&str>,
        enabled: bool,
        status: Option<String>,
    ) -> bool {
        let mut applied = false;
        let matches = |skill: &SkillBinding| {
            if provider == SkillProvider::Claude && source.is_some() {
                skill.source.as_deref() == source
            } else {
                skill.path == path
            }
        };
        if SkillProvider::from_model(self.selected_model_name()) == provider {
            for skill in &mut self.skills {
                if matches(skill) {
                    skill.enabled = enabled;
                    applied = true;
                }
            }
            self.rebuild_completion_catalog();
        }
        if let Some(PendingInteraction::SkillsPicker {
            provider: open_provider,
            skills,
            notice,
            ..
        }) = self.pending.as_mut()
            && *open_provider == provider
        {
            for skill in skills {
                if matches(skill) {
                    skill.enabled = enabled;
                    applied = true;
                }
            }
            *notice = status;
        }
        applied
    }

    pub fn open_skills_picker(
        &mut self,
        provider: SkillProvider,
        response: &Value,
        notice: Option<String>,
    ) {
        self.update_skills_for_provider(provider, response);
        let mut skills = parse_skill_bindings(response);
        skills.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
                .then_with(|| left.path.cmp(&right.path))
        });
        self.pending = Some(PendingInteraction::SkillsPicker {
            provider,
            selected: 0,
            skills,
            errors: parse_skill_errors(response),
            notice,
        });
    }

    pub fn update_plugins(&mut self, response: &Value) {
        let model = self.selected_model_name().to_owned();
        self.update_plugins_for_model(response, &model);
    }

    pub fn update_plugins_for_model(&mut self, response: &Value, model: &str) {
        let provider = ModelProvider::from_model(model);
        if provider == self.selected_provider() {
            self.mentions = parse_plugin_mentions(response);
        }
        let plugins = PluginCatalog::from_value(response).installed_panel_items();
        if let Some(snapshot) = self.integration_snapshot_mut(provider) {
            snapshot.plugins = Some(plugins);
            snapshot.plugin_error = None;
        }
        if provider == self.selected_provider() {
            self.rebuild_completion_catalog();
        }
    }

    pub fn update_mcp_servers_for_model(&mut self, response: &Value, model: &str) {
        let provider = ModelProvider::from_model(model);
        let mut servers = McpServerInfo::list_from_value(response);
        let active_names = servers
            .iter()
            .map(|server| server.name.clone())
            .collect::<HashSet<_>>();
        self.mcp_failures.retain(|failure| {
            failure.provider != provider || !active_names.contains(&failure.name)
        });
        let mut items = servers
            .drain(..)
            .map(|server| server.panel_item())
            .collect::<Vec<_>>();
        items.extend(
            self.mcp_failures
                .iter()
                .filter(|failure| failure.provider == provider)
                .map(|failure| IntegrationItemView {
                    name: failure.name.clone(),
                    state: IntegrationItemState::Inactive,
                    detail: "실패".to_owned(),
                }),
        );
        items.sort_by(|left, right| {
            integration_item_order(left.state)
                .cmp(&integration_item_order(right.state))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        if let Some(snapshot) = self.integration_snapshot_mut(provider) {
            snapshot.mcp = Some(items);
            snapshot.mcp_error = response
                .get("unavailableReason")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
    }

    pub fn note_mcp_query_error_for_model(&mut self, error: impl Into<String>, model: &str) {
        if let Some(snapshot) = self.integration_snapshot_mut(ModelProvider::from_model(model)) {
            snapshot.mcp_error = Some(error.into());
        }
    }

    pub fn note_plugin_query_error_for_model(&mut self, error: impl Into<String>, model: &str) {
        if let Some(snapshot) = self.integration_snapshot_mut(ModelProvider::from_model(model)) {
            snapshot.plugin_error = Some(error.into());
        }
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
            label: self.permission_mode().label().to_owned(),
            accent: self.permission_mode().accent(),
            model: self.selected_model_name().to_owned(),
            response_length: self.response_length_label().to_owned(),
            response_display_mode: self.response_display_mode.label().to_owned(),
            fast_mode: self.effective_fast_mode(),
            claude_permission: self.claude_permission_badge(),
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

    pub fn provider_handoff_plan(&self) -> Option<String> {
        let plan = self.plan_summary.as_ref()?;
        if plan.explanation.is_none() && plan.steps.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        if let Some(explanation) = plan.explanation.as_deref() {
            lines.push(explanation.to_owned());
        }
        lines.extend(plan.steps.iter().map(|step| {
            let status = match step.status {
                PlanStepStatus::Completed => "완료",
                PlanStepStatus::InProgress => "진행 중",
                PlanStepStatus::Pending => "대기",
            };
            format!("- [{status}] {}", step.text)
        }));
        Some(lines.join("\n"))
    }

    pub fn pending_provider_handoff_blocks(&self) -> Vec<ProviderHandoffBlock> {
        self.committed_before_current_prompt()
            .iter()
            .filter_map(ProviderHandoffBlock::from_block)
            .collect()
    }

    pub fn last_pending_handoff_block_id(&self) -> u64 {
        self.committed_before_current_prompt()
            .iter()
            .map(Block::id)
            .max()
            .unwrap_or_default()
    }

    fn committed_before_current_prompt(&self) -> &[Block] {
        let end = self.committed.len().saturating_sub(usize::from(
            self.committed
                .last()
                .is_some_and(|block| matches!(block.kind, BlockKind::User)),
        ));
        &self.committed[..end]
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

    /// The permission mode Claude runs the next turn under. Only Claude sessions
    /// have one; a Codex thread keeps its Fast badge in the same slot.
    pub fn claude_permission_mode(&self) -> Option<ClaudePermissionMode> {
        self.selected_model_name()
            .starts_with("claude:")
            .then_some(self.claude_permission_mode)
    }

    /// The mode a Claude session should open under, badge or no badge. Resuming a
    /// Claude thread from a Codex session still has to send it, and that is the
    /// case [`Self::claude_permission_mode`] deliberately hides.
    pub fn claude_permission_mode_setting(&self) -> ClaudePermissionMode {
        self.claude_permission_mode
    }

    fn claude_permission_badge(&self) -> Option<PermissionBadge> {
        self.claude_permission_mode().map(|mode| PermissionBadge {
            label: mode.label().to_owned(),
            tone: mode.tone(),
        })
    }

    /// Steps to the next mode and reports it, so the caller can tell the runtime.
    /// The badge itself is the feedback — a notice under the composer would only
    /// flash away while the reading it duplicates stays on screen.
    pub fn cycle_claude_permission_mode(&mut self) -> Action {
        let auto_available = self.claude_auto_mode_available();
        let mode = self
            .claude_permission_mode
            .next(auto_available, self.bypass_permissions_allowed);
        self.choose_claude_permission_mode(mode)
    }

    pub fn set_claude_permission_mode(&mut self, mode: ClaudePermissionMode) {
        self.claude_permission_mode = mode;
    }

    pub fn apply_claude_permission_status(&mut self, status: &Value) {
        self.apply_claude_permission_policy(status);
        let mode = status
            .get("defaultMode")
            .and_then(Value::as_str)
            .and_then(ClaudePermissionMode::from_wire)
            .unwrap_or_default();
        self.claude_auto_mode_confirmed |= mode == ClaudePermissionMode::Auto;
        self.claude_permission_mode = if (mode == ClaudePermissionMode::Auto
            && self.selected_model_name().starts_with("claude:")
            && !self.claude_auto_mode_available())
            || (mode == ClaudePermissionMode::BypassPermissions && !self.bypass_permissions_allowed)
        {
            ClaudePermissionMode::Default
        } else {
            mode
        };
    }

    pub fn apply_claude_permission_policy(&mut self, status: &Value) {
        self.bypass_permissions_allowed = status
            .get("bypassAvailable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.claude_auto_mode_disabled = status
            .get("autoDisabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    }

    pub fn set_claude_auto_mode_disabled(&mut self, disabled: bool) {
        self.claude_auto_mode_disabled = disabled;
    }

    pub fn open_claude_permissions(&mut self, status: &Value, notice: Option<String>) {
        self.apply_claude_permission_policy(status);
        if let Some(notice) = notice {
            self.composer_notice = Some((notice, Instant::now()));
        }
        let mut entries = status
            .get("rules")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|rule| {
                Some(ClaudePermissionEntry {
                    behavior: rule.get("behavior")?.as_str()?.to_owned(),
                    value: rule.get("rule")?.as_str()?.to_owned(),
                    source: rule.get("source")?.as_str()?.to_owned(),
                    mutable: rule
                        .get("mutable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect::<Vec<_>>();
        entries.extend(
            status
                .get("directories")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|directory| {
                    Some(ClaudePermissionEntry {
                        behavior: "directory".to_owned(),
                        value: directory.get("directory")?.as_str()?.to_owned(),
                        source: directory.get("source")?.as_str()?.to_owned(),
                        mutable: directory
                            .get("mutable")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                }),
        );
        let denials = status
            .get("denials")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|denial| {
                Some(ClaudePermissionDenial {
                    tool: denial.get("tool")?.as_str()?.to_owned(),
                    reason: denial
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    input: denial.get("input").cloned().unwrap_or_else(|| json!({})),
                })
            })
            .collect();
        self.pending = Some(PendingInteraction::ClaudePermissionsPanel {
            tab: 0,
            selected: 0,
            entries,
            denials,
            retry: None,
            rules_locked: status
                .get("rulesLocked")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }

    fn claude_auto_mode_available(&self) -> bool {
        !self.claude_auto_mode_disabled
            && self
                .selected_model()
                .is_some_and(|model| model.supports_auto_mode)
    }

    fn normalize_claude_permission_mode_for_selected_model(&mut self) {
        if (self.claude_permission_mode == ClaudePermissionMode::Auto
            && self.selected_model_name().starts_with("claude:")
            && !self.claude_auto_mode_available())
            || (self.claude_permission_mode == ClaudePermissionMode::BypassPermissions
                && !self.bypass_permissions_allowed)
        {
            self.claude_permission_mode = ClaudePermissionMode::Default;
        }
    }

    pub fn open_claude_permission_picker(&mut self) {
        let auto_available = self.claude_auto_mode_available();
        self.pending = Some(PendingInteraction::ClaudePermissionPicker {
            selected: self
                .claude_permission_mode
                .picker_index(auto_available, self.bypass_permissions_allowed),
        });
    }

    /// Opens the Vibe preset picker, the way `/vibemode` and the composer's Vibe
    /// badge do — a menu instead of an instant cycle.
    pub fn open_vibe_mode_picker(&mut self) {
        self.pending = Some(PendingInteraction::VibeModePicker {
            selected: self.vibe_mode.picker_index(),
            vibe: self.vibe_mode,
            response: self.response_length,
            shell: self.shell_display_mode,
            diff: self.diff_display_mode,
        });
    }

    /// Opens the Response display picker, the way `/Response` and the composer's
    /// Response badge do.
    pub fn open_response_display_picker(&mut self) {
        self.open_setting_picker(
            DisplaySetting::Response,
            match self.response_display_mode {
                ResponseDisplayMode::All => 0,
                ResponseDisplayMode::Completed => 1,
            },
        );
    }

    fn apply_claude_permission_picker(&mut self, selected: usize) -> Action {
        let Some(mode) = ClaudePermissionMode::choices(
            self.claude_auto_mode_available(),
            self.bypass_permissions_allowed,
        )
        .get(selected)
        .copied() else {
            return Action::None;
        };
        self.choose_claude_permission_mode(mode)
    }

    fn choose_claude_permission_mode(&mut self, mode: ClaudePermissionMode) -> Action {
        if mode == ClaudePermissionMode::Auto && !self.claude_auto_mode_confirmed {
            self.pending = Some(PendingInteraction::ClaudeAutoModeConsent { selected: 0 });
            return Action::None;
        }
        self.claude_permission_mode = mode;
        Action::SetClaudePermissionMode(mode)
    }

    fn apply_claude_auto_mode_consent(&mut self, selected: usize) -> Action {
        match selected {
            0 => {
                self.claude_auto_mode_confirmed = true;
                self.claude_permission_mode = ClaudePermissionMode::Auto;
                Action::SetClaudePermissionMode(ClaudePermissionMode::Auto)
            }
            2 => {
                self.claude_auto_mode_disabled = true;
                Action::DisableClaudeAutoMode
            }
            _ => Action::None,
        }
    }

    pub fn effective_fast_mode(&self) -> bool {
        self.fast_mode
            && self
                .selected_model()
                .is_some_and(|model| model.fast_service_tier.is_some())
    }

    /// The explicit choice confirms the new service tier in the transcript as
    /// well as updating the persistent composer badge.
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
        // Ctrl+C spent on a copy is not a quit attempt, so it cannot leave the
        // quit armed behind for the next Ctrl+C to trip over.
        self.disarm_quit();
        self.composer_notice = Some(("• Copied to clipboard".to_owned(), Instant::now()));
    }

    /// Arms the quit and puts up the warning that spends the same window.
    fn arm_quit(&mut self) {
        self.quit_armed_at = Some(Instant::now());
        self.activity_notice = Some((
            "• Ctrl+C 한 번 더 누르면 종료합니다.".to_owned(),
            Instant::now(),
            QUIT_ARM_WINDOW,
        ));
    }

    /// Any deliberate input other than a second Ctrl+C cancels the pending quit.
    pub fn disarm_quit(&mut self) {
        self.quit_armed_at = None;
    }

    fn quit_armed(&self) -> bool {
        self.quit_armed_at
            .is_some_and(|armed_at| armed_at.elapsed() < QUIT_ARM_WINDOW)
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

    /// Restores the safe-return marker when resuming the parent fails. The side
    /// thread stays current, and Esc/Ctrl+C must remain unable to reach the
    /// ordinary interrupt/quit branches on the retry.
    pub fn restore_side_parent(&mut self, thread_id: String, turn: Option<(String, Instant)>) {
        self.side_parent = Some(SideParent {
            thread_id,
            turn: turn.map(|(id, started_at)| ParentTurn { id, started_at }),
        });
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
        self.commit_welcome_card();
        self.transient_status = Some("Side · Ctrl+C to return".to_owned());
        self.committed.push(Block::new(
            BlockKind::System,
            "Side conversation",
            "Ephemeral fork · Ctrl+C to return to the main thread",
        ));
    }

    pub fn begin_side_prompt(&mut self, text: String) {
        self.commit_welcome_card();
        self.reset_turn_item_tracking();
        let prompt = Block::new(BlockKind::User, self.selected_model_name(), text);
        self.turn_prompt_ids.push(prompt.id());
        self.committed.push(prompt);
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
        self.resume_id = String::new();
        let cwd = plain_folder(cwd);
        if self.cwd != cwd {
            self.cwd = cwd;
            self.workspace_entries.clear();
            self.rebuild_completion_catalog();
        }
        self.turn_id = None;
        self.pending_interrupt = false;
        self.busy = false;
        self.end_compaction();
        self.turn_started_at = None;
        self.active.clear();
        self.active_order.clear();
        self.shell_batches.clear();
        self.reset_turn_item_tracking();
        self.show_welcome = true;
        self.restore_session_side_panel();
        self.restore_session_modes();
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
        self.normalize_claude_permission_mode_for_selected_model();
    }

    /// Rebuilds the transcript from a resumed thread. `rollout` fills in what
    /// `thread/resume` omits — shell runs above all — placing each one back where
    /// it ran rather than at the end of its turn.
    pub fn load_history(&mut self, thread: &Value, rollout: Option<&Rollout>) {
        let Some(turns) = thread.get("turns").and_then(Value::as_array) else {
            return;
        };
        let prompt_history = turns
            .iter()
            .filter_map(|turn| turn.get("items").and_then(Value::as_array))
            .flatten()
            .filter_map(user_message_text)
            .collect::<Vec<_>>();
        self.editor.replace_history(prompt_history);
        self.turn_interrupted = false;
        self.last_completed_duration = turns.iter().rev().find_map(|turn| {
            let started = turn.get("startedAt")?.as_i64()?;
            let completed = turn.get("completedAt")?.as_i64()?;
            u64::try_from(completed.checked_sub(started)?)
                .ok()
                .map(Duration::from_secs)
        });
        // Neither the turn nor the rollout names a model for every prompt — a
        // Codex thread carries no per-turn model at all. The thread reopened on
        // one model, so use it rather than dropping the prompt's marker back to
        // the plain accent.
        let resumed_model = self.selected_model_name().to_owned();
        for turn in turns {
            let Some(items) = turn.get("items").and_then(Value::as_array) else {
                continue;
            };
            for mut block in merged_turn_blocks(&self.cwd, turn, items, rollout) {
                if matches!(block.kind, BlockKind::User) && block.title == UNKNOWN_PROMPT_MODEL {
                    block.title = resumed_model.clone();
                }
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
        } else if let Some(plan) = plan_snapshot_from_history(turns) {
            self.restore_plan_snapshot(&plan);
        }
    }

    fn restore_plan_snapshot(&mut self, plan: &PlanSnapshot) {
        self.plan_summary = Some(PlanSummary {
            explanation: plan.explanation.clone(),
            steps: plan
                .steps
                .iter()
                .enumerate()
                .map(|(index, step)| PlanStep {
                    text: numbered_plan_step(&step.text, index),
                    status: match step.status.as_str() {
                        "completed" => PlanStepStatus::Completed,
                        "in_progress" => PlanStepStatus::InProgress,
                        _ => PlanStepStatus::Pending,
                    },
                    started_at: None,
                    elapsed: step.elapsed_ms.map(Duration::from_millis),
                })
                .collect(),
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        });
        self.plan_turn_id = None;
    }

    pub fn set_turn_started(&mut self, turn_id: String) {
        if self.turn_id.as_deref() != Some(turn_id.as_str()) {
            self.reset_turn_item_tracking();
            self.plan_turn_id = None;
        }
        self.turn_id = Some(turn_id);
        self.busy = true;
        if !self.pending_interrupt {
            self.turn_interrupted = false;
        }
        self.last_completed_duration = None;
        self.turn_progress_at = Some(Instant::now());
        self.stall_probe_at = None;
        // A prompt held back while the session was still starting has been counting
        // since the user pressed Enter, so keep that clock rather than restarting it.
        self.turn_started_at.get_or_insert_with(Instant::now);
    }

    /// A turn that has gone quiet for a while is worth asking the runtime about.
    /// The answer decides whether the wait ends — silence alone never does, since
    /// a long think looks exactly the same from here.
    pub fn take_stall_probe(&mut self) -> Option<String> {
        if !self.busy || self.compacting() {
            self.stall_probe_at = None;
            return None;
        }
        let turn_id = self.turn_id.clone()?;
        if self.turn_progress_at?.elapsed() < TURN_STALL_SILENCE {
            return None;
        }
        if self
            .stall_probe_at
            .is_some_and(|sent| sent.elapsed() < TURN_STALL_SILENCE)
        {
            return None;
        }
        self.stall_probe_at = Some(Instant::now());
        Some(turn_id)
    }

    /// The runtime reports the turn we are still waiting on is over: its
    /// `turn/completed` never arrived, so end the wait exactly as that would have.
    pub fn resolve_stall_probe(&mut self, turn_id: &str) -> bool {
        if !self.busy || self.turn_id.as_deref() != Some(turn_id) {
            return false;
        }
        self.stall_probe_at = None;
        let params = json!({ "threadId": self.thread_id.clone() });
        self.handle_notification("turn/completed", &params);
        self.push_notice(
            BlockKind::Warning,
            "응답 종료 알림 누락",
            "턴은 이미 끝났는데 종료 알림이 오지 않아 진행 표시를 정리했습니다.",
        );
        true
    }

    fn reset_turn_item_tracking(&mut self) {
        self.turn_response_started = false;
        self.completed_item_ids.clear();
        self.seen_operation_signatures.clear();
        self.turn_shell_results.clear();
        self.turn_shell_anchor = None;
        self.turn_shell_duration_ms = None;
        self.turn_file_changes.clear();
        self.turn_file_change_anchor = None;
        self.turn_response_blocks.clear();
        self.turn_prompt_ids.clear();
        self.response_grouped = false;
        self.response_collapse = None;
    }

    fn push_unique_operation(&mut self, block: Block) -> bool {
        if let Some(signature) = operation_signature(&block)
            && !self.seen_operation_signatures.insert(signature)
        {
            return false;
        }
        push_latest_thinking(&mut self.committed, block);
        true
    }

    fn collapse_completed_response(&mut self) {
        let assistant_indices = self
            .turn_response_blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                matches!(block.kind, BlockKind::Assistant).then_some(index)
            })
            .collect::<Vec<_>>();
        if self.response_grouped || assistant_indices.is_empty() {
            return;
        }
        let final_index = assistant_indices
            .iter()
            .copied()
            .rfind(|&index| {
                self.turn_response_blocks[index].assistant_phase() == AssistantPhase::FinalAnswer
            })
            .unwrap_or(*assistant_indices.last().expect("assistant block"));
        let progress = self.turn_response_blocks[..final_index]
            .iter()
            .filter(|block| {
                is_context_compaction(block)
                    || (matches!(block.kind, BlockKind::Assistant) && !block.body.trim().is_empty())
            })
            .cloned()
            .collect::<Vec<_>>();
        if progress.is_empty() {
            return;
        }
        let groups = progress_groups_for_prompt_ids(progress, &self.turn_prompt_ids);
        let Some(last_group) = groups.last() else {
            return;
        };
        self.response_collapse = (self.vibe_mode == VibeMode::SuperVibe
            && self.response_display_mode == ResponseDisplayMode::Completed)
            .then(|| ResponseCollapseTransition {
                group_id: last_group.id(),
                started_at: Instant::now(),
            });
        self.response_grouped = true;
        self.committed.extend(groups);
    }

    fn collapse_progress_before_next_answer(&mut self) {
        if self.vibe_mode != VibeMode::SuperVibe
            || self.response_display_mode != ResponseDisplayMode::Completed
        {
            return;
        }
        let progress = self
            .turn_response_blocks
            .iter()
            .filter(|block| {
                is_context_compaction(block)
                    || (matches!(block.kind, BlockKind::Assistant) && !block.body.trim().is_empty())
            })
            .cloned()
            .collect::<Vec<_>>();
        if progress.is_empty() {
            return;
        }

        self.response_grouped = true;
        self.committed.extend(progress_groups_for_prompt_ids(
            progress,
            &self.turn_prompt_ids,
        ));
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
        self.end_compaction();
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
        let provider = self.selected_provider();
        for failure in std::mem::take(&mut self.mcp_failures) {
            if failure.provider == provider {
                picker.apply_failure(&failure.name, failure.detail);
            } else {
                self.mcp_failures.push(failure);
            }
        }
        self.pending = Some(PendingInteraction::McpPicker(picker));
    }

    pub fn apply_mcp_enabled(
        &mut self,
        provider: SkillProvider,
        name: &str,
        enabled: bool,
        notice: impl Into<String>,
    ) -> bool {
        if SkillProvider::from_model(self.selected_model_name()) == provider
            && let Some(PendingInteraction::McpPicker(picker)) = self.pending.as_mut()
        {
            picker.apply_enabled(name, enabled, notice);
            return true;
        }
        false
    }

    pub fn finish_mcp_reconnect(
        &mut self,
        provider: SkillProvider,
        response: &Value,
        notice: String,
    ) {
        self.update_mcp_servers_for_model(response, provider.model_hint());
        if SkillProvider::from_model(self.selected_model_name()) == provider
            && matches!(self.pending, Some(PendingInteraction::McpPicker(_)))
        {
            self.open_mcp_picker(McpServerInfo::list_from_value(response), Some(notice));
        }
    }

    pub fn open_provider_picker(&mut self, catalog: &Value) {
        self.pending = Some(PendingInteraction::ProviderPicker(
            ProviderPicker::from_value(catalog),
        ));
    }

    pub fn open_provider_loading(&mut self) {
        self.pending = Some(PendingInteraction::ProviderLoading);
    }

    pub fn open_provider_oauth(
        &mut self,
        provider_id: String,
        provider_name: String,
        method: usize,
        url: String,
        instructions: String,
        callback_method: &str,
    ) {
        self.pending = if callback_method == "code" {
            Some(PendingInteraction::ProviderOAuthCode {
                provider_id,
                provider_name,
                method,
                url,
                instructions,
                editor: Editor::default(),
                validation: None,
            })
        } else {
            Some(PendingInteraction::ProviderOAuthWaiting {
                provider_name,
                url,
                instructions,
            })
        };
    }

    pub fn provider_connected(&mut self, provider_name: &str) {
        self.pending = None;
        self.push_notice(
            BlockKind::System,
            "Provider connected",
            format!("{provider_name} 연결이 완료되었습니다."),
        );
    }

    pub fn provider_connection_failed(&mut self, message: impl Into<String>) {
        self.pending = None;
        self.push_notice(BlockKind::Error, "Provider 연결 실패", message.into());
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

    pub fn apply_plugin_enabled(
        &mut self,
        provider: SkillProvider,
        id: &str,
        enabled: bool,
        notice: impl Into<String>,
    ) -> bool {
        if SkillProvider::from_model(self.selected_model_name()) == provider
            && let Some(PendingInteraction::PluginPicker(picker)) = self.pending.as_mut()
        {
            picker.apply_enabled(id, enabled, notice);
            return true;
        }
        false
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
            picker.apply_failure(&name, detail.clone());
        }
        let provider = self.selected_provider();
        self.mcp_failures
            .retain(|failure| failure.provider != provider || failure.name != name);
        self.mcp_failures.push(McpFailure {
            provider,
            name: name.clone(),
            detail,
        });
        if let Some(snapshot) = self.selected_integration_snapshot_mut()
            && let Some(items) = snapshot.mcp.as_mut()
        {
            items.retain(|item| item.name != name);
            items.push(IntegrationItemView {
                name,
                state: IntegrationItemState::Inactive,
                detail: "실패".to_owned(),
            });
            items.sort_by(|left, right| {
                integration_item_order(left.state)
                    .cmp(&integration_item_order(right.state))
                    .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            });
        }
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
        self.quit_armed_at = None;
        self.plan_summary = None;
        self.response_collapse = None;
        self.turn_response_blocks.clear();
        self.response_grouped = false;
        self.subagents.clear();
        self.subagent_logs.clear();
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
        if self.vibe_mode == VibeMode::SuperVibe {
            committed.retain(|block| !is_plan_block(block));
        }
        committed
    }

    fn plan_is_active(&self) -> bool {
        self.busy && self.turn_id.is_some() && self.plan_turn_id == self.turn_id
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
                    || (self.vibe_mode == VibeMode::SuperVibe && is_plan_block(&item.block))
                    || is_empty_thinking(&item.block)
                {
                    return None;
                }
                if let Some(signature) = operation_signature(&item.block)
                    && !operation_signatures.insert(signature)
                {
                    return None;
                }
                Some(LiveBlockView {
                    block: &item.block,
                    revision: item.revision,
                })
            })
            .collect::<Vec<_>>();
        View {
            live_blocks,
            overlay: self.overlay_view(),
            plan_summary: self.plan_summary.as_ref(),
            response_collapse: self.response_collapse_view(),
            fold_progress_groups: self.vibe_mode == VibeMode::SuperVibe
                && self.response_display_mode == ResponseDisplayMode::Completed,
            plan_active: self.plan_is_active(),
            plan_shimmer_phase: self.plan_shimmer_phase(),
            plan_effort: self
                .active_turn_effort
                .as_deref()
                .or(self.pending_turn_effort.as_deref()),
            editor: &self.editor,
            composer_images: &self.composer_images,
            queued_prompts: self.queued_prompts.iter().cloned().collect(),
            subagents: self
                .subagents
                .iter()
                .map(|running| SubagentView {
                    id: running.id.clone(),
                    name: running.name.clone(),
                    description: running.description.clone(),
                    tool: running.tool.clone(),
                    elapsed: running.started_at.elapsed(),
                })
                .collect(),
            composer_placeholder: if self.provider_switch_pending() {
                "Enter: queue for switched provider · Tab: queue"
            } else if self.busy {
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
            waiting_for_response: self.busy
                && !self.turn_response_started
                && self.last_assistant_markdown.is_some(),
            stream_fade_tail: self.stream_fade_tail.round() as usize,
            activity_progress_phase: self.compaction_progress_phase(),
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
            side_panel_width: self.side_panel_stage.width(),
            side_panel_prompts_expanded: self.side_panel_prompts_expanded,
            side_panel_integrations: self.side_panel_integration_views(),
        }
    }

    pub fn tick(&mut self) -> bool {
        self.render_tick().redraw
    }

    pub fn render_tick(&mut self) -> TickResult {
        let mut full_redraw = false;
        // Windows Terminal owns the visible IME preedit until composition is
        // committed as a key event. Any spinner repaint hides and restores the
        // cursor, erasing that preedit and exposing our placeholder again. Keep
        // periodic paints still while an inline answer owns the cursor; the
        // committed character event will paint the new text normally.
        let inline_answer_active = self.buffers_pending_text_input();
        // Compaction animates the same row a turn does, so it keeps the frame
        // loop alive even on a runtime that reports no turn while it runs.
        let animating = self.busy || self.compacting();
        // A background Claude agent outlives its parent turn. Its elapsed label
        // changes once a second, and the narrow animation path does not repaint
        // these rows, so only that boundary asks for a full frame.
        let mut subagent_elapsed_changed = false;
        for running in &mut self.subagents {
            let elapsed = running.started_at.elapsed().as_secs();
            if running.painted_elapsed_secs != elapsed {
                running.painted_elapsed_secs = elapsed;
                subagent_elapsed_changed = true;
            }
        }
        let plan_shimmer_active = self.plan_shimmer_phase().is_some();
        if self.plan_shimmer_started_at.is_some() && !plan_shimmer_active {
            self.plan_shimmer_started_at = None;
            full_redraw = true;
        }
        let response_collapse_active = self.response_collapse_view().is_some();
        if self.response_collapse.is_some() && !response_collapse_active {
            self.response_collapse = None;
            full_redraw = true;
        }
        if animating {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
        }
        if self.status_metadata_refreshed_at.elapsed().as_secs() >= 3 {
            let branch = read_git_branch(&self.cwd);
            let (five_hour_percent, weekly_percent, five_hour_reset_at) = if self
                .selected_model()
                .is_some_and(|model| model.model.starts_with("claude:"))
            {
                (
                    self.five_hour_percent,
                    self.weekly_percent,
                    self.five_hour_reset_at,
                )
            } else {
                read_codex_usage()
            };
            let five_hour_remaining = remaining_label(five_hour_reset_at, unix_now());
            let fast_mode = read_fast_mode();
            full_redraw = self.branch != branch
                || self.five_hour_percent != five_hour_percent
                || self.weekly_percent != weekly_percent
                || self.five_hour_remaining != five_hour_remaining
                || self.fast_mode != fast_mode;
            self.branch = branch;
            self.five_hour_percent = five_hour_percent;
            self.weekly_percent = weekly_percent;
            self.five_hour_reset_at = five_hour_reset_at;
            self.five_hour_remaining = five_hour_remaining;
            self.fast_mode = fast_mode;
            self.status_metadata_refreshed_at = Instant::now();
        }
        if self
            .composer_notice
            .as_ref()
            .is_some_and(|(_, shown_at)| shown_at.elapsed() >= NOTICE_TTL)
        {
            self.composer_notice = None;
            full_redraw = true;
        }
        if self
            .activity_notice
            .as_ref()
            .is_some_and(|(_, shown_at, ttl)| shown_at.elapsed() >= *ttl)
        {
            self.activity_notice = None;
            full_redraw = true;
        }
        if !self.quit_armed() {
            self.quit_armed_at = None;
        }
        TickResult {
            redraw: !inline_answer_active
                && (animating
                    || subagent_elapsed_changed
                    || plan_shimmer_active
                    || response_collapse_active
                    || full_redraw),
            animation_only: !inline_answer_active
                && (animating || plan_shimmer_active)
                && !response_collapse_active
                && !subagent_elapsed_changed
                && !full_redraw,
        }
    }

    pub fn animation_view(&self) -> AnimationView<'_> {
        AnimationView {
            activity: self.activity(),
            activity_model: self.activity_model(),
            activity_phase: self.activity_phase(),
            waiting_for_response: self.busy
                && !self.turn_response_started
                && self.last_assistant_markdown.is_some(),
            activity_progress_phase: self.compaction_progress_phase(),
            plan_summary: self.plan_summary.as_ref(),
            plan_active: self.plan_is_active(),
            plan_shimmer_phase: self.plan_shimmer_phase(),
            plan_effort: self
                .active_turn_effort
                .as_deref()
                .or(self.pending_turn_effort.as_deref()),
            composer_notice: self
                .composer_notice
                .as_ref()
                .map(|(notice, _)| notice.as_str()),
            composer_mode: Some(self.composer_mode()),
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.handle_inserted_text(text, true);
    }

    /// Applies text to the exact pending question that owned the key when it
    /// entered the short IME/paste buffer. A selection change or a later prompt
    /// makes the token stale, so the text is dropped instead of appearing on a
    /// different line.
    pub fn handle_buffered_prompt_text(&mut self, target: &str, text: &str) {
        if self.pending_text_input_target().as_deref() != Some(target) {
            return;
        }
        self.disarm_quit();
        if let Some(PendingInteraction::UserInput { editor, .. }) = &mut self.pending {
            editor.insert_str(text);
        }
    }

    /// Buffered composer text belongs to the draft that owned its key even if a
    /// server prompt opened before the short classification delay expired.
    pub fn handle_buffered_composer_text(&mut self, text: &str, pasted: bool) {
        self.disarm_quit();
        let old_text = self.editor.text();
        let binding_count = self.selected_completion_bindings.len();
        if pasted {
            self.editor.insert_paste_str(text);
        } else {
            self.editor.insert_str(text);
        }
        self.command_selection = 0;
        self.sync_selected_completion_bindings(&old_text, binding_count);
    }

    fn handle_inserted_text(&mut self, text: &str, pasted: bool) {
        // Pasted or buffered text is input, not a quit, so it disarms like a keypress.
        self.disarm_quit();
        let old_text = self.editor.text();
        let binding_count = self.selected_completion_bindings.len();
        match &mut self.pending {
            Some(PendingInteraction::UserInput {
                questions,
                current,
                selected,
                editor,
                ..
            }) if questions
                .get(*current)
                .is_some_and(|question| user_input_text_focused(question, *selected)) =>
            {
                editor.insert_str(text);
            }
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
            Some(PendingInteraction::ProviderPicker(picker)) => picker.handle_paste(text),
            Some(PendingInteraction::ProviderOAuthCode { editor, .. }) => editor.insert_str(text),
            Some(PendingInteraction::ClaudePermissionRuleInput { editor, .. }) => {
                editor.insert_str(text)
            }
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

    /// 호스트(DevezCode 등)가 입력창 텍스트를 bracketed paste로 pty에 쓰면
    /// 승인 프롬프트의 답이 Key 이벤트 대신 Paste로 도착해 그대로 버려졌다.
    /// 답을 기다리는 프롬프트에서 한 글자짜리 paste는 그 키로 재해석한다.
    pub fn paste_as_prompt_answer(&mut self, text: &str) -> Option<Action> {
        if !matches!(
            self.pending,
            Some(
                PendingInteraction::Confirm { .. }
                    | PendingInteraction::McpApproval(_)
                    | PendingInteraction::McpUrl { .. }
            )
        ) {
            return None;
        }
        let mut chars = text.trim().chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            return None;
        };
        Some(self.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)))
    }

    /// Deletes the composer characters a drag selected, along with any attachment
    /// whose placeholder the drag covered. Reports whether the range was
    /// consumed, so a Backspace that finds nothing selected still falls through
    /// to the one before the cursor.
    pub fn delete_composer_selection(&mut self, range: std::ops::Range<usize>) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let mut first_image = None;
        let mut images = 0;
        let mut seen = 0;
        for (index, &ch) in self.editor.chars().iter().enumerate() {
            if ch != ATTACHMENT_PLACEHOLDER {
                continue;
            }
            if range.contains(&index) {
                first_image = first_image.or(Some(seen));
                images += 1;
            }
            seen += 1;
        }
        let old_text = self.editor.text();
        let binding_count = self.selected_completion_bindings.len();
        if !self.editor.delete_display_range(range) {
            return false;
        }
        self.sync_selected_completion_bindings(&old_text, binding_count);
        if let Some(first) = first_image {
            let start = first.min(self.composer_images.len());
            let end = (first + images).min(self.composer_images.len());
            self.composer_images.drain(start..end);
        }
        self.command_selection = 0;
        self.disarm_quit();
        true
    }

    /// Applies a cursor position resolved from a click on the main composer.
    /// Blocking prompts own keyboard focus, so the composer beneath one ignores
    /// stray clicks just as selection deletion does.
    pub fn move_composer_cursor(&mut self, index: usize) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.editor.move_to_display_index(index)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if !(key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
            self.disarm_quit();
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // In a full-screen `/btw` fork these are navigation keys, never process
        // controls. Resolve them before pending prompts and the ordinary
        // interrupt/quit branches so neither can consume the parent's turn.
        if self.side_parent.is_some()
            && (key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && ctrl))
        {
            self.disarm_quit();
            return Action::ReturnFromSide;
        }
        // Windows Korean IMEs may turn one Ctrl+Backspace chord into a stream
        // of repeat records while dismantling a composed syllable. A word
        // delete must stay one atomic editor operation.
        if matches!(key.kind, KeyEventKind::Repeat)
            && ((key.code == KeyCode::Backspace && key.modifiers.contains(KeyModifiers::CONTROL))
                || key.code == KeyCode::Char('\u{8}'))
        {
            return Action::None;
        }
        if self.pending.is_some() {
            return self.handle_pending_key(key);
        }

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
            self.suggestions_dismissed_text = None;
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
                        self.suggestions_dismissed_text = Some(self.editor.text());
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
                KeyCode::Esc => {
                    self.suggestions_dismissed_text = Some(self.editor.text());
                    self.command_selection = 0;
                    return Action::None;
                }
                _ => {}
            }
        }

        match key.code {
            // Shift+Tab arrives as BackTab on terminals without the Kitty keyboard
            // protocol, and as a shifted Tab with it. Either way it cycles Claude's
            // permission mode, as it does in the CLI — so the shifted Tab is claimed
            // here, ahead of the plain Tab that queues a prompt during a turn.
            KeyCode::BackTab | KeyCode::Tab if key.code == KeyCode::BackTab || shift => {
                if self.claude_permission_mode().is_some() {
                    return self.cycle_claude_permission_mode();
                }
                Action::Tick(true)
            }
            // Alt+V cycles the vibe preset from the keyboard, the badge's own
            // shortcut. It works mid-turn too; the response settings it carries
            // then apply to the next request.
            KeyCode::Char('v') | KeyCode::Char('V') if alt && !ctrl => {
                let (shell, diff) = self.cycle_vibe_mode();
                Action::PersistVibeDisplayModes {
                    vibe: self.vibe_mode,
                    response: self.response_length,
                    shell,
                    diff,
                }
            }
            // Alt+W folds the plan panel. Shift+Space is kept as the historical
            // chord, but a Korean IME claims it as its own language toggle, so it
            // never reaches us on those systems — Alt+W is the one that always does.
            KeyCode::Char('w') | KeyCode::Char('W') if alt && !ctrl => {
                self.toggle_plan_summary();
                Action::Tick(true)
            }
            // Alt+P steps the docked side panel through its widths and closes
            // it again on the fourth press. A bare capital would be swallowed
            // by the composer's typed-text buffer, so the chord carries Alt to
            // reach this branch at all.
            KeyCode::Char('p') | KeyCode::Char('P') if alt && !ctrl => {
                self.cycle_side_panel();
                Action::None
            }
            // The terminal still reports a space for Shift+Space, so the composer
            // must not also type one.
            KeyCode::Char(' ') if shift && !ctrl && !alt => {
                self.toggle_plan_summary();
                Action::Tick(true)
            }
            KeyCode::Char('c') if ctrl => {
                if self.busy {
                    if self.quit_armed() {
                        Action::Quit
                    } else {
                        self.arm_quit();
                        self.request_interrupt()
                    }
                } else if self.editor.is_empty()
                    && self.composer_images.is_empty()
                    && self.side_parent.is_some()
                {
                    Action::ReturnFromSide
                } else if self.editor.is_empty() && self.composer_images.is_empty() {
                    if self.quit_armed() {
                        Action::Quit
                    } else {
                        self.arm_quit();
                        Action::None
                    }
                } else {
                    // Clearing the composer is the action the user asked for, so the
                    // quit does not stay armed behind it.
                    self.disarm_quit();
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
            KeyCode::Delete if ctrl => {
                if let Some(index) = self.editor.attachment_at_cursor() {
                    self.editor.delete_word_right();
                    self.composer_images.remove(index);
                } else {
                    self.editor.delete_word_right();
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
                let (session, session_label) =
                    approval_session_choice(params, json!({ "decision": "acceptForSession" }));
                self.pending = Some(PendingInteraction::Approval {
                    id,
                    title: params
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|title| !title.is_empty())
                        .unwrap_or("명령 실행을 허용할까요?")
                        .to_owned(),
                    detail,
                    selected: 0,
                    once: json!({ "decision": "accept" }),
                    session,
                    session_label,
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
                let (session, session_label) =
                    approval_session_choice(params, json!({ "decision": "acceptForSession" }));
                self.pending = Some(PendingInteraction::Approval {
                    id,
                    title: params
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|title| !title.is_empty())
                        .unwrap_or("파일 변경을 허용할까요?")
                        .to_owned(),
                    detail,
                    selected: 0,
                    once: json!({ "decision": "accept" }),
                    session,
                    session_label,
                    decline: json!({ "decision": "decline" }),
                });
                Action::None
            }
            "item/permissions/requestApproval" => {
                let requested = params
                    .get("permissions")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let mut detail = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .filter(|reason| !reason.is_empty())
                    .map(|reason| vec![reason.to_owned()])
                    .unwrap_or_default();
                detail.extend(permission_detail(&requested));
                let (session, session_label) = approval_session_choice(
                    params,
                    json!({
                        "permissions": params.get("permissions").cloned().unwrap_or_else(|| json!({})),
                        "scope": "session"
                    }),
                );
                self.pending = Some(PendingInteraction::Approval {
                    id,
                    title: params
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|title| !title.is_empty())
                        .unwrap_or("추가 권한을 허용할까요?")
                        .to_owned(),
                    detail,
                    selected: 0,
                    once: json!({ "permissions": requested, "scope": "turn" }),
                    session,
                    session_label,
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
                self.pending = Some(PendingInteraction::UserInput {
                    id,
                    questions,
                    current: 0,
                    selected: 0,
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

    /// The runtime names one Claude session two ways — bare, and `claude:`-prefixed.
    /// Both spell this thread, and reading the bare one as somebody else's used to
    /// throw away a whole turn: its output never painted and its completion never
    /// ended the wait.
    fn names_this_thread(&self, thread_id: &str) -> bool {
        thread_id == self.thread_id
            || crate::claude::raw_thread_id(thread_id)
                == crate::claude::raw_thread_id(&self.thread_id)
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
            .filter(|thread_id| !self.names_this_thread(thread_id))
        {
            self.note_background_turn(thread_id, method);
            return;
        }
        // Anything this thread says counts as the turn still being alive, so the
        // stall probe only fires on a wait that has genuinely gone silent.
        self.turn_progress_at = Some(Instant::now());
        // A finishing notice arrives while the last words of the answer are still
        // waiting their turn on screen. Handling it now would flush them all at
        // once, which lands as a block of text appearing at full strength — the
        // one moment the paced reveal was meant to remove. Hold it instead, and
        // keep holding everything after it so the order is preserved.
        if !self.held_notifications.is_empty() || self.should_hold_for_stream(method) {
            self.hold_notification(method, params);
            return;
        }
        self.dispatch_notification(method, params);
    }

    /// Whether this notice has to wait for the text still being revealed.
    fn should_hold_for_stream(&self, method: &str) -> bool {
        matches!(
            method,
            "item/completed" | "turn/completed" | "turn/failed" | "turn/aborted"
        ) && self.stream_text_pending()
    }

    fn stream_text_pending(&self) -> bool {
        self.active
            .values()
            .any(|active| !active.pace.pending.is_empty())
    }

    fn hold_notification(&mut self, method: &str, params: &Value) {
        if self.held_notifications.is_empty() {
            self.held_since = Some(Instant::now());
            self.held_final_frame_ticks = 0;
        }
        self.held_notifications
            .push((method.to_owned(), params.clone()));
    }

    /// Deliver the held notices once the text they follow has all appeared, or
    /// once the wait has run long enough that holding them is the bigger problem.
    fn release_held_notifications(&mut self, revealed_text: bool) -> (bool, bool) {
        if self.held_notifications.is_empty() {
            self.held_final_frame_ticks = 0;
            return (false, false);
        }
        let expired = self
            .held_since
            .is_some_and(|since| since.elapsed() >= HELD_NOTIFICATION_LIMIT);
        if self.stream_text_pending() && !expired {
            return (false, false);
        }
        if self.stream_text_pending() {
            self.flush_stream_text();
            self.held_final_frame_ticks = FINAL_STREAM_FRAME_TICKS;
            return (true, false);
        }
        if revealed_text {
            // The chunk that emptied the queue has not been rendered yet. Keep
            // the item live so its final wrapped height is painted before the
            // same body moves into transcript history.
            self.held_final_frame_ticks = FINAL_STREAM_FRAME_TICKS;
            return (false, false);
        }
        if self.held_final_frame_ticks > 0 {
            self.held_final_frame_ticks -= 1;
            return (false, false);
        }
        self.held_since = None;
        self.held_final_frame_ticks = 0;
        for (method, params) in std::mem::take(&mut self.held_notifications) {
            self.dispatch_notification(&method, &params);
        }
        (false, true)
    }

    fn dispatch_notification(&mut self, method: &str, params: &Value) {
        if matches!(
            method,
            "item/completed" | "turn/completed" | "turn/failed" | "turn/aborted"
        ) {
            self.flush_stream_text();
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
            // This thread's conversation now lives in a session other than the one it
            // is named after — a provider switch, or a Claude session the CLI persisted
            // under a rotated uuid. The screen keeps the thread's id; what the host
            // records for the next launch has to follow the conversation.
            "thread/rebound" => {
                if let Some(next) = params
                    .get("newThreadId")
                    .and_then(Value::as_str)
                    .filter(|next| !next.is_empty())
                {
                    self.resume_id = next.to_owned();
                }
            }
            // Plan or auth mode changed underneath us; pull the fresh values.
            "account/updated" => self.account_refresh_due = true,
            "claude/account/updated" => {
                let account = params.get("account").filter(|value| !value.is_null());
                let usage = params.get("usage").filter(|value| !value.is_null());
                if let Some(label) = account
                    .and_then(|account| account.get("email"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    self.account = label.to_owned();
                }
                self.set_account_plan(AccountPlan::from_claude(account, usage));
            }
            "claude/permissionMode/rejected" => {
                if let Some(mode) = params
                    .get("effectivePermissionMode")
                    .and_then(Value::as_str)
                    .and_then(ClaudePermissionMode::from_wire)
                {
                    self.set_claude_permission_mode(mode);
                }
                self.push_notice(
                    BlockKind::Warning,
                    "권한 모드 전환 거부",
                    params
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Claude 설정 또는 관리 정책에서 이 모드를 허용하지 않습니다."),
                );
            }
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
                let turn_error = params
                    .get("turn")
                    .and_then(|turn| turn.get("error"))
                    .filter(|error| !error.is_null());
                let turn_status = params
                    .get("turn")
                    .and_then(|turn| turn.get("status"))
                    .and_then(Value::as_str);
                let successful = !self.turn_interrupted
                    && turn_error.is_none()
                    && !matches!(turn_status, Some("failed" | "aborted" | "interrupted"));
                self.busy = false;
                // A runtime that compacts inside a turn ends the spinner here even
                // if it never announced the boundary.
                self.end_compaction();
                self.turn_id = None;
                self.pending_interrupt = false;
                let mut retained_logs = self
                    .subagents
                    .iter()
                    .map(|running| running.id.clone())
                    .collect::<HashSet<_>>();
                if let Some(PendingInteraction::SubagentTranscript { id, .. }) = &self.pending {
                    retained_logs.insert(id.clone());
                }
                self.subagent_logs
                    .retain(|id, _| retained_logs.contains(id));
                if !self.turn_interrupted || self.last_completed_duration.is_none() {
                    self.last_completed_duration =
                        self.turn_started_at.map(|started| started.elapsed());
                }
                self.turn_started_at = None;
                if let Some(error) = turn_error {
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
                if successful {
                    self.collapse_completed_response();
                }
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
                    .enumerate()
                    .filter_map(|(index, step)| {
                        let text = numbered_plan_step(step.get("step")?.as_str()?, index);
                        let status = match step.get("status").and_then(Value::as_str) {
                            Some("completed") => PlanStepStatus::Completed,
                            Some("inProgress") => PlanStepStatus::InProgress,
                            _ => PlanStepStatus::Pending,
                        };
                        let previous = self.plan_summary.as_ref().and_then(|summary| {
                            summary.steps.iter().find(|previous| previous.text == text)
                        });
                        let started_at = match status {
                            PlanStepStatus::InProgress => previous
                                .and_then(|previous| previous.started_at)
                                .or_else(|| Some(Instant::now())),
                            PlanStepStatus::Completed => {
                                previous.and_then(|previous| previous.started_at)
                            }
                            PlanStepStatus::Pending => None,
                        };
                        let elapsed = if status == PlanStepStatus::Completed {
                            previous
                                .and_then(|previous| previous.elapsed)
                                .or_else(|| started_at.map(|started| started.elapsed()))
                        } else {
                            None
                        };
                        Some(PlanStep {
                            text,
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
                    .is_none_or(|summary| summary.expanded);
                let elapsed = if !steps.is_empty()
                    && steps
                        .iter()
                        .all(|step| step.status == PlanStepStatus::Completed)
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
                self.plan_turn_id = self.turn_id.clone();
                self.plan_shimmer_started_at = Some(Instant::now());
                self.commit_welcome_card();
            }
            "turn/subagents/updated" => {
                self.subagents = params
                    .get("subagents")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        let id = entry.get("id").and_then(Value::as_str)?;
                        let previous = self.subagents.iter().find(|running| running.id == id);
                        let started_at = previous
                            .map(|running| running.started_at)
                            .unwrap_or_else(Instant::now);
                        let painted_elapsed_secs = previous
                            .map(|running| running.painted_elapsed_secs)
                            .unwrap_or_else(|| started_at.elapsed().as_secs());
                        Some(RunningSubagent {
                            id: id.to_owned(),
                            name: entry
                                .get("name")
                                .and_then(Value::as_str)
                                .filter(|name| !name.trim().is_empty())
                                .unwrap_or("agent")
                                .to_owned(),
                            description: entry
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            tool: entry
                                .get("tool")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            started_at,
                            painted_elapsed_secs,
                        })
                    })
                    .collect();
            }
            "turn/subagent/line" => {
                if let Some(parent) = params.get("parentToolUseId").and_then(Value::as_str)
                    && let Some(line) = params.get("line")
                    && let Some(text) = line
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                {
                    let kind = line.get("kind").and_then(Value::as_str).unwrap_or("text");
                    let text = match kind {
                        "tool" => format!("⏺ {text}"),
                        "result" => format!("  ⎿ {text}"),
                        "error" => format!("  ⎿ 오류: {text}"),
                        _ => text.to_owned(),
                    };
                    let log = self.subagent_logs.entry(parent.to_owned()).or_default();
                    log.push(SubagentLogLine {
                        text,
                        muted: kind != "text",
                    });
                    if log.len() > SUBAGENT_LOG_LIMIT {
                        log.drain(..log.len() - SUBAGENT_LOG_LIMIT);
                    }
                }
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
                let title = params
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex");
                self.append_delta(params, BlockKind::Assistant, title);
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
                    active.revision = active.revision.wrapping_add(1);
                }
            }
            "item/mcpToolCall/progress" => {
                if let Some(item_id) = params.get("itemId").and_then(Value::as_str)
                    && let Some(message) = params.get("message").and_then(Value::as_str)
                {
                    let active = self.ensure_active(item_id, BlockKind::Tool, "MCP");
                    append_capped(&mut active.block.body, message);
                    active.revision = active.revision.wrapping_add(1);
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
                        if !self.cost_restore_pending
                            && let Some(ledger) = self.cost_ledger.as_mut()
                        {
                            ledger.record_cumulative(&model, self.token_totals);
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
                let provider = params
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex");
                self.committed.push(Block::new(
                    if retry {
                        BlockKind::Warning
                    } else {
                        BlockKind::Error
                    },
                    if retry {
                        "재시도 중"
                    } else {
                        match provider {
                            "Codex" => "Codex 오류",
                            "Claude" => "Claude 오류",
                            _ => "OpenCode 오류",
                        }
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
            "thread/compacted" => {
                self.end_compaction();
                let block = Block::new(
                    BlockKind::System,
                    "Context compacted",
                    "대화 컨텍스트가 압축되었습니다.",
                );
                if self.push_unique_operation(block.clone()) {
                    self.turn_response_blocks.push(block);
                }
            }
            _ => {}
        }
    }

    fn submit_editor(&mut self) -> Action {
        // A fresh install has picked no runtime yet, so the first prompt opens
        // the picker instead of guessing one. Nothing leaves the composer, and
        // slash commands still run — `/provider` among them.
        let text = self.editor.text();
        let command = text.starts_with('/') && !text.contains('\n');
        if self.provider_choice_pending && !self.any_provider_connected() && !command {
            self.open_runtime_picker();
            return Action::None;
        }
        let display = submission_display(&self.editor.display_text(), self.composer_images.len());
        let text = self.editor.take_for_submit().unwrap_or_default();
        self.submit_text(text, display)
    }

    pub fn start_queued_prompt(&mut self, text: String) -> Action {
        self.submit_text(text.clone(), text)
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

    fn submit_text(&mut self, text: String, display: String) -> Action {
        if text.is_empty() && self.composer_images.is_empty() {
            return Action::None;
        }
        if text.starts_with('/') && !text.contains('\n') {
            return self.run_slash_command(&text);
        }
        if self.provider_switch_pending() {
            self.queued_prompts.push_back(text);
            return Action::None;
        }
        // A prompt sent mid-compaction would race the summary the runtime is
        // still writing, so it waits in the queue like one sent during a turn.
        if self.compacting() && !self.busy {
            self.queued_prompts.push_back(text);
            return Action::None;
        }
        self.commit_welcome_card();
        let steering = self.busy;
        if !steering {
            self.reset_turn_item_tracking();
        }
        let prompt = Block::new(BlockKind::User, self.selected_model_name(), display);
        self.turn_prompt_ids.push(prompt.id());
        self.committed.push(prompt);
        if steering {
            Action::Steer(text)
        } else {
            self.busy = true;
            // Time the turn from Enter, not from the server's acknowledgement: a
            // prompt held back by a starting session would otherwise read 0s.
            self.turn_started_at = Some(Instant::now());
            Action::Submit(text)
        }
    }

    pub(crate) fn run_slash_command(&mut self, command: &str) -> Action {
        let parts = command.split_whitespace().collect::<Vec<_>>();
        let using_claude = self.selected_model_name().starts_with("claude:");
        match parts.first().copied().unwrap_or_default() {
            "/help" => {
                let provider_help = if crate::open_code::PROVIDER_ENABLED {
                    "/connect  OpenCode provider 연결\n"
                } else {
                    Default::default()
                };
                let login_help = if using_claude {
                    "/login  Claude 로그인 방법\n/logout  Claude 로그아웃 방법"
                } else {
                    "/login  ChatGPT 계정 로그인\n/logout  계정 연결 해제"
                };
                let fast_help = if using_claude {
                    ""
                } else {
                    "/fast [on|off]  Fast 서비스 티어 선택\n"
                };
                let effort_help = if self
                    .selected_model()
                    .is_some_and(|model| !model.efforts.is_empty())
                {
                    "/effort [LEVEL]  추론 수준\n"
                } else {
                    Default::default()
                };
                self.committed.push(Block::new(
                    BlockKind::System,
                    "Commands",
                    format!("/provider [claude|codex]  Claude·Codex provider 전환과 연결 사용/미사용\n/model [MODEL] [EFFORT]  현재 provider의 모델과 effort 선택\n{provider_help}{fast_help}{effort_help}/Response [All|Completed]  응답 압축 방식\n/permissions  현재 provider 권한 규칙 관리\n/shell [hide|collapse|expand]  Shell 표시 방식\n/diff [hide|collapse|expand]  Diff 표시 방식\n/theme [minimal|soft|dark]  화면 테마\n/statusline  하단 상태줄 항목 표시\n/side-panel  우측 사이드패널 크기와 적용 범위 선택\n/mcp [reconnect [NAME]|login NAME]  MCP 서버 탐색과 관리\n/plugins [install|uninstall|enable|disable NAME]  플러그인 탐색과 관리\n/plugins marketplace [add SOURCE|remove NAME|upgrade]  마켓플레이스 관리\n/reload-plugins  플러그인 변경을 현재 세션에 적용\n/skills [enable|disable NAME]  Skill 관리\n/btw [MESSAGE]  임시 사이드 대화\n/compact  컨텍스트 압축\n/copy  마지막 답변 복사\n/resume [SESSION]  이전 세션 선택\n/continue  /resume 별칭\n/new  새 대화\n{login_help}\n/status  현재 설정\n/usage  사용 한도\n/clear  화면 정리\n/quit  종료\n\n$  Plugin·Skill·App 검색\n@  Plugin·Skill·파일·폴더 검색\nEsc 또는 Ctrl+C  실행 중단\nCtrl+Enter / Shift+Enter  줄바꿈\nShift+Space 또는 Alt+W  작업 단계 접기/펴기\nAlt+P  우측 사이드패널 크기 전환(닫힘→24→36→48)\nShift+Tab  Claude 권한 모드 전환"),
                ));
                Action::None
            }
            "/provider" if parts.len() == 1 => {
                self.open_runtime_picker();
                Action::None
            }
            "/provider" if parts.len() == 2 => match parts[1].to_ascii_lowercase().as_str() {
                "claude" => self.apply_runtime_choice(0),
                "codex" => self.apply_runtime_choice(1),
                _ => {
                    self.committed.push(Block::new(
                        BlockKind::Error,
                        "Usage",
                        "/provider [claude|codex]",
                    ));
                    Action::None
                }
            },
            "/provider" => {
                self.committed.push(Block::new(
                    BlockKind::Error,
                    "Usage",
                    "/provider [claude|codex]",
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
                        if self.effective_fast_mode() { 0 } else { 1 },
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
                let provider_models = self.current_provider_model_indices();
                let index = query
                    .parse::<usize>()
                    .ok()
                    .and_then(|number| number.checked_sub(1))
                    .and_then(|index| provider_models.get(index).copied())
                    .or_else(|| {
                        provider_models
                            .iter()
                            .copied()
                            .find(|index| self.models[*index].matches_query(query))
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
                if self
                    .selected_model()
                    .is_none_or(|model| model.efforts.is_empty())
                {
                    self.committed.push(Block::new(
                        BlockKind::Error,
                        "Effort unavailable",
                        "현재 모델은 reasoning effort를 지원하지 않습니다.",
                    ));
                    return Action::None;
                }
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
            "/permissions" if parts.len() == 1 && using_claude => {
                Action::OpenClaudePermissions(None)
            }
            "/permissions" if parts.len() == 1 => {
                self.committed.push(Block::new(
                    BlockKind::System,
                    "Permissions",
                    "Codex는 현재 Full Access 권한 프로필을 사용합니다.",
                ));
                Action::None
            }
            "/permissions" => {
                self.committed
                    .push(Block::new(BlockKind::Error, "Usage", "/permissions"));
                Action::None
            }
            "/Response" | "/response" if parts.len() == 1 => {
                self.open_response_display_picker();
                Action::None
            }
            "/Response" | "/response" if parts.len() == 2 => {
                self.set_display_setting(DisplaySetting::Response, parts[1])
            }
            "/Response" | "/response" => {
                self.committed.push(Block::new(
                    BlockKind::Error,
                    "Usage",
                    "/Response [All|Completed]",
                ));
                Action::None
            }
            "/vibemode" if parts.len() == 1 => {
                self.open_vibe_mode_picker();
                Action::None
            }
            "/vibemode" => {
                self.committed
                    .push(Block::new(BlockKind::Error, "Usage", "/vibemode"));
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
                Action::ReconnectMcp(None)
            }
            "/mcp" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("reconnect") => {
                Action::ReconnectMcp(Some(parts[2..].join(" ")))
            }
            "/mcp" => {
                self.committed.push(Block::new(
                    BlockKind::Error,
                    "Usage",
                    "/mcp, /mcp reconnect [SERVER] 또는 /mcp login SERVER",
                ));
                Action::None
            }
            "/connect" if !crate::open_code::PROVIDER_ENABLED => {
                self.committed.push(Block::new(
                    BlockKind::Warning,
                    "OpenCode provider 비활성화",
                    "후속 개선 전까지 OpenCode provider는 사용할 수 없습니다.",
                ));
                Action::None
            }
            "/connect" if parts.len() == 1 => Action::ConnectProvider,
            "/connect" => {
                self.committed
                    .push(Block::new(BlockKind::Error, "Usage", "/connect"));
                Action::None
            }
            "/login" if using_claude => {
                self.push_notice(
                    BlockKind::System,
                    "Claude 로그인",
                    "터미널에서 `claude auth login`을 실행한 뒤 Devez Vibe를 다시 시작하세요.",
                );
                Action::None
            }
            "/login" => {
                self.open_login_picker();
                Action::None
            }
            "/logout" if using_claude => {
                self.push_notice(
                    BlockKind::Warning,
                    "Claude 로그아웃",
                    "터미널에서 `claude auth logout`을 실행한 뒤 Devez Vibe를 다시 시작하세요.",
                );
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
                    provider: SkillProvider::from_model(self.selected_model_name()),
                    name: parts[2..].join(" "),
                    enabled: true,
                }
            }
            "/skills" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("disable") => {
                Action::SetSkill {
                    provider: SkillProvider::from_model(self.selected_model_name()),
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
            "/compact" if self.compacting() => {
                self.committed.push(Block::new(
                    BlockKind::Warning,
                    "압축 중",
                    "이미 컨텍스트를 압축하고 있습니다.",
                ));
                Action::None
            }
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
            "/side-panel" if parts.len() == 1 => {
                self.pending = Some(PendingInteraction::SidePanelPicker {
                    stage_index: self.side_panel_stage.index(),
                });
                Action::None
            }
            "/side-panel" => {
                self.committed
                    .push(Block::new(BlockKind::Error, "Usage", "/side-panel"));
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
                let provider = self.selected_provider().label();
                let permissions = self
                    .claude_permission_mode()
                    .map(|mode| format!("{} ({})", mode.label(), mode.wire()))
                    .unwrap_or_else(|| {
                        format!(
                            "{} ({})",
                            self.permission_mode().label(),
                            self.permission_mode().profile()
                        )
                    });
                let connections = format!(
                    "Claude {} · Codex {}",
                    if self.claude_provider_enabled {
                        "연결됨"
                    } else {
                        "연결 안 함"
                    },
                    if self.codex_provider_enabled {
                        "연결됨"
                    } else {
                        "연결 안 함"
                    }
                );
                self.committed.push(Block::new(
                    BlockKind::System,
                    "Status",
                    format!(
                        "thread: {}\nprovider: {provider}\nconnections: {connections}\nmodel: {model}\neffort: {}\ntheme: {}\npermissions: {permissions}\ncwd: {}",
                        self.thread_id,
                        self.selected_effort,
                        theme::current().display_name(),
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
        // 한글 IME가 켜진 채 승인 프롬프트에 답하면 y/a/n이 두벌식 자모로
        // 도착한다. 자유 입력이 없는 프롬프트에서만 같은 키로 취급한다.
        let hotkey = match key.code {
            KeyCode::Char('ㅛ') => KeyCode::Char('y'),
            KeyCode::Char('ㅁ') => KeyCode::Char('a'),
            KeyCode::Char('ㅜ') => KeyCode::Char('n'),
            KeyCode::Char('ㅐ') => KeyCode::Char('o'),
            code => code,
        };
        match pending {
            PendingInteraction::ModelPicker {
                mut model_index,
                mut effort_index,
            } => {
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Up => {
                        model_index = self.move_model_index(model_index, -1);
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Char('k') if !ctrl && !alt => {
                        model_index = self.move_model_index(model_index, -1);
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Char('p') if ctrl => {
                        model_index = self.move_model_index(model_index, -1);
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Down => {
                        model_index = self.move_model_index(model_index, 1);
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Char('j') if !ctrl && !alt => {
                        model_index = self.move_model_index(model_index, 1);
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Char('n') if ctrl => {
                        model_index = self.move_model_index(model_index, 1);
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Char(ch) if !ctrl && !alt && ('1'..='9').contains(&ch) => {
                        let index = ch.to_digit(10).unwrap_or_default() as usize - 1;
                        if let Some(model_index) =
                            self.current_provider_model_indices().get(index).copied()
                        {
                            self.open_model_scope(
                                model_index,
                                self.effort_index_for_model(model_index),
                            );
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
                    .unwrap_or(1)
                    .max(1);
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
            PendingInteraction::SidePanelPicker { mut stage_index } => {
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Left | KeyCode::Up => {
                        stage_index = stage_index.saturating_sub(1);
                    }
                    KeyCode::Char('p') if ctrl => {
                        stage_index = stage_index.saturating_sub(1);
                    }
                    KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                        stage_index = (stage_index + 1).min(SidePanelStage::CHOICES.len() - 1);
                    }
                    KeyCode::Char('n') if ctrl => {
                        stage_index = (stage_index + 1).min(SidePanelStage::CHOICES.len() - 1);
                    }
                    KeyCode::Char(ch) if !ctrl && !alt && ('1'..='4').contains(&ch) => {
                        let index = ch.to_digit(10).unwrap_or(1) as usize - 1;
                        self.open_side_panel_scope(SidePanelStage::CHOICES[index]);
                        return Action::None;
                    }
                    KeyCode::Enter => {
                        self.open_side_panel_scope(SidePanelStage::CHOICES[stage_index]);
                        return Action::None;
                    }
                    _ => {}
                }
                self.pending = Some(PendingInteraction::SidePanelPicker { stage_index });
                Action::None
            }
            PendingInteraction::SidePanelScope {
                stage,
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
                        return self.apply_side_panel_scope(stage, ModelScope::CHOICES[index]);
                    }
                    KeyCode::Enter => {
                        return self.apply_side_panel_scope(stage, ModelScope::CHOICES[selected]);
                    }
                    _ => {}
                }
                self.pending = Some(PendingInteraction::SidePanelScope { stage, selected });
                Action::None
            }
            PendingInteraction::RuntimePicker { mut selected } => {
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Up | KeyCode::Left => selected = selected.saturating_sub(1),
                    KeyCode::Char('p') if ctrl => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                        selected = (selected + 1).min(RUNTIME_CHOICES.len() - 1);
                    }
                    KeyCode::Char('n') if ctrl => {
                        selected = (selected + 1).min(RUNTIME_CHOICES.len() - 1);
                    }
                    KeyCode::Char(' ') => {
                        self.pending = Some(PendingInteraction::RuntimePicker { selected });
                        return self.toggle_runtime_connection(selected);
                    }
                    KeyCode::Char(ch @ '1'..='2') => {
                        let row = ch.to_digit(10).unwrap_or(1) as usize - 1;
                        return self.apply_runtime_choice(row);
                    }
                    KeyCode::Enter => return self.apply_runtime_choice(selected),
                    _ => {}
                }
                self.pending = Some(PendingInteraction::RuntimePicker { selected });
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
            PendingInteraction::ClaudePermissionPicker { mut selected } => {
                let count = ClaudePermissionMode::choices(
                    self.claude_auto_mode_available(),
                    self.bypass_permissions_allowed,
                )
                .len();
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
                            return self.apply_claude_permission_picker(index);
                        }
                    }
                    KeyCode::Enter => return self.apply_claude_permission_picker(selected),
                    _ => {}
                }
                self.pending = Some(PendingInteraction::ClaudePermissionPicker { selected });
                Action::None
            }
            PendingInteraction::ClaudeAutoModeConsent { mut selected } => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('n') if !ctrl && !alt => return Action::None,
                    KeyCode::Up | KeyCode::Left => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                        selected = (selected + 1).min(2);
                    }
                    KeyCode::Char(ch @ '1'..='3') if !ctrl && !alt => {
                        return self.apply_claude_auto_mode_consent(
                            ch.to_digit(10).unwrap_or(1) as usize - 1,
                        );
                    }
                    KeyCode::Enter => return self.apply_claude_auto_mode_consent(selected),
                    _ => {}
                }
                self.pending = Some(PendingInteraction::ClaudeAutoModeConsent { selected });
                Action::None
            }
            PendingInteraction::ClaudePermissionsPanel {
                mut tab,
                mut selected,
                entries,
                denials,
                mut retry,
                rules_locked,
            } => {
                let restore = |state: &mut Self, tab, selected, entries, denials, retry| {
                    state.pending = Some(PendingInteraction::ClaudePermissionsPanel {
                        tab,
                        selected,
                        entries,
                        denials,
                        retry,
                        rules_locked,
                    });
                };
                match key.code {
                    KeyCode::Esc => {
                        return retry
                            .and_then(|index| denials.get(index))
                            .map(|denial| Action::RetryClaudePermissionDenial {
                                tool: denial.tool.clone(),
                                input: denial.input.clone(),
                            })
                            .unwrap_or(Action::None);
                    }
                    KeyCode::Left => {
                        tab = tab.saturating_sub(1);
                        selected = 0;
                    }
                    KeyCode::Right | KeyCode::Tab => {
                        tab = (tab + 1).min(CLAUDE_PERMISSION_TABS.len() - 1);
                        selected = 0;
                    }
                    KeyCode::Up | KeyCode::Char('p') if key.code == KeyCode::Up || ctrl => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('n') if key.code == KeyCode::Down || ctrl => {
                        let count = if let Some(behavior) = claude_permission_behavior(tab) {
                            entries
                                .iter()
                                .filter(|entry| entry.behavior == behavior)
                                .count()
                        } else {
                            denials.len()
                        };
                        selected = (selected + 1).min(count.saturating_sub(1));
                    }
                    KeyCode::Char('a') if tab < 4 && !rules_locked && !ctrl && !alt => {
                        return {
                            self.pending = Some(PendingInteraction::ClaudePermissionScopePicker {
                                behavior: claude_permission_behavior(tab)
                                    .expect("permission rule tab")
                                    .to_owned(),
                                selected: if tab == 2 { 2 } else { 1 },
                            });
                            Action::None
                        };
                    }
                    KeyCode::Delete | KeyCode::Char('d') if tab < 4 && !ctrl && !alt => {
                        let target = claude_permission_behavior(tab).and_then(|behavior| {
                            entries
                                .iter()
                                .filter(|entry| entry.behavior == behavior)
                                .nth(selected)
                                .cloned()
                        });
                        if let Some(target) = target.filter(|entry| entry.mutable) {
                            return Action::UpdateClaudePermission {
                                action: "remove",
                                behavior: target.behavior,
                                value: target.value,
                                destination: target.source,
                            };
                        }
                        self.composer_notice = Some((
                            if rules_locked {
                                "관리 정책에서 권한 규칙 변경을 잠갔습니다."
                            } else {
                                "관리되는 권한 규칙은 제거할 수 없습니다."
                            }
                            .to_owned(),
                            Instant::now(),
                        ));
                    }
                    KeyCode::Char('r') if tab == 4 && !ctrl && !alt && selected < denials.len() => {
                        retry = (retry != Some(selected)).then_some(selected);
                    }
                    _ => {}
                }
                restore(self, tab, selected, entries, denials, retry);
                Action::None
            }
            PendingInteraction::ClaudePermissionScopePicker {
                behavior,
                mut selected,
            } => {
                match key.code {
                    KeyCode::Esc => return Action::OpenClaudePermissions(None),
                    KeyCode::Left | KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                        selected = (selected + 1).min(CLAUDE_PERMISSION_SCOPES.len() - 1);
                    }
                    KeyCode::Char(ch @ '1'..='3') if !ctrl && !alt => {
                        selected = ch.to_digit(10).unwrap_or(1) as usize - 1;
                        self.pending = Some(PendingInteraction::ClaudePermissionRuleInput {
                            behavior,
                            destination: CLAUDE_PERMISSION_SCOPES[selected].1.to_owned(),
                            editor: Editor::default(),
                        });
                        return Action::None;
                    }
                    KeyCode::Enter => {
                        self.pending = Some(PendingInteraction::ClaudePermissionRuleInput {
                            behavior,
                            destination: CLAUDE_PERMISSION_SCOPES[selected].1.to_owned(),
                            editor: Editor::default(),
                        });
                        return Action::None;
                    }
                    _ => {}
                }
                self.pending =
                    Some(PendingInteraction::ClaudePermissionScopePicker { behavior, selected });
                Action::None
            }
            PendingInteraction::ClaudePermissionRuleInput {
                behavior,
                destination,
                mut editor,
            } => match key.code {
                KeyCode::Esc => Action::OpenClaudePermissions(None),
                KeyCode::Enter => match editor.take_for_submit() {
                    Some(value) => Action::UpdateClaudePermission {
                        action: "add",
                        behavior,
                        value,
                        destination,
                    },
                    None => {
                        self.pending = Some(PendingInteraction::ClaudePermissionRuleInput {
                            behavior,
                            destination,
                            editor,
                        });
                        Action::None
                    }
                },
                KeyCode::Backspace => {
                    editor.backspace();
                    self.pending = Some(PendingInteraction::ClaudePermissionRuleInput {
                        behavior,
                        destination,
                        editor,
                    });
                    Action::None
                }
                KeyCode::Char(ch) if !ctrl && !alt => {
                    editor.insert(ch);
                    self.pending = Some(PendingInteraction::ClaudePermissionRuleInput {
                        behavior,
                        destination,
                        editor,
                    });
                    Action::None
                }
                _ => {
                    self.pending = Some(PendingInteraction::ClaudePermissionRuleInput {
                        behavior,
                        destination,
                        editor,
                    });
                    Action::None
                }
            },
            PendingInteraction::VibeModePicker {
                mut selected,
                vibe,
                response,
                shell,
                diff,
            } => {
                let moved = match key.code {
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
                    KeyCode::Left | KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                        true
                    }
                    KeyCode::Char('p') if ctrl => {
                        selected = selected.saturating_sub(1);
                        true
                    }
                    KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                        selected = (selected + 1).min(VibeMode::PICKER_CHOICES.len() - 1);
                        true
                    }
                    KeyCode::Char('n') if ctrl => {
                        selected = (selected + 1).min(VibeMode::PICKER_CHOICES.len() - 1);
                        true
                    }
                    _ => false,
                };
                if moved {
                    self.apply_vibe_mode(VibeMode::PICKER_CHOICES[selected]);
                }
                self.pending = Some(PendingInteraction::VibeModePicker {
                    selected,
                    vibe,
                    response,
                    shell,
                    diff,
                });
                Action::None
            }
            PendingInteraction::SubagentTranscript { id, offset } => match key.code {
                KeyCode::Esc | KeyCode::Enter => Action::None,
                KeyCode::Up | KeyCode::Char('k') if !ctrl && !alt => {
                    self.pending = Some(PendingInteraction::SubagentTranscript {
                        id,
                        offset: offset.saturating_sub(1),
                    });
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') if !ctrl && !alt => {
                    let offset = (offset + 1).min(self.subagent_log_max_offset(&id));
                    self.pending = Some(PendingInteraction::SubagentTranscript { id, offset });
                    Action::None
                }
                _ => {
                    self.pending = Some(PendingInteraction::SubagentTranscript { id, offset });
                    Action::None
                }
            },
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
            PendingInteraction::SkillsPicker {
                provider,
                mut selected,
                skills,
                errors,
                notice,
            } => {
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                        return Action::OpenSkills {
                            provider: provider.other(),
                            notice: None,
                        };
                    }
                    KeyCode::Up | KeyCode::Char('k') if !ctrl && !alt => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Char('p') if ctrl => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') if !ctrl && !alt => {
                        selected = (selected + 1).min(skills.len().saturating_sub(1));
                    }
                    KeyCode::Char('n') if ctrl => {
                        selected = (selected + 1).min(skills.len().saturating_sub(1));
                    }
                    KeyCode::Char(' ') | KeyCode::Enter => {
                        if let Some(skill) = skills.get(selected).cloned() {
                            let enabled = !skill.enabled;
                            self.pending = Some(PendingInteraction::SkillsPicker {
                                provider,
                                selected,
                                skills,
                                errors,
                                notice,
                            });
                            self.apply_skill_enabled(
                                provider,
                                &skill.path,
                                skill.source.as_deref(),
                                enabled,
                                Some(format!("{} · 저장 중", skill.name)),
                            );
                            return Action::SetSkillEnabled {
                                provider,
                                name: skill.name,
                                path: skill.path,
                                source: skill.source,
                                scope: skill.scope,
                                enabled,
                            };
                        }
                    }
                    _ => {}
                }
                self.pending = Some(PendingInteraction::SkillsPicker {
                    provider,
                    selected,
                    skills,
                    errors,
                    notice,
                });
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
                    KeyCode::Char(ch) if ('1'..='6').contains(&ch) => {
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
                McpPickerResult::Login(name) => {
                    self.pending = Some(PendingInteraction::McpPicker(picker));
                    Action::McpLogin(name)
                }
                McpPickerResult::Reconnect(name) => {
                    self.pending = Some(PendingInteraction::McpPicker(picker));
                    Action::ReconnectMcp(Some(name))
                }
                McpPickerResult::Toggle { name, enabled } => {
                    let provider = SkillProvider::from_model(self.selected_model_name());
                    picker.begin_enabled(&name, enabled, format!("{name} · 저장 중"));
                    self.pending = Some(PendingInteraction::McpPicker(picker));
                    Action::SetMcpEnabled {
                        provider,
                        name,
                        enabled,
                    }
                }
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
                        let id = plugin.id.clone();
                        let label = plugin.display_name.clone();
                        picker.apply_enabled(&id, enabled, format!("{label} · 저장 중"));
                        self.pending = Some(PendingInteraction::PluginPicker(picker));
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
            PendingInteraction::ProviderLoading => {
                self.pending = Some(PendingInteraction::ProviderLoading);
                Action::None
            }
            PendingInteraction::ProviderPicker(mut picker) => match picker.handle_key(key) {
                ProviderPickerResult::None => {
                    self.pending = Some(PendingInteraction::ProviderPicker(picker));
                    Action::None
                }
                ProviderPickerResult::Cancel => Action::None,
                ProviderPickerResult::Submit(request) => Action::SubmitProviderAuth(request),
            },
            PendingInteraction::ProviderOAuthCode {
                provider_id,
                provider_name,
                method,
                url,
                instructions,
                mut editor,
                mut validation,
            } => match key.code {
                KeyCode::Char('o') if !ctrl && !alt => {
                    let target = url.clone();
                    self.pending = Some(PendingInteraction::ProviderOAuthCode {
                        provider_id,
                        provider_name,
                        method,
                        url,
                        instructions,
                        editor,
                        validation,
                    });
                    Action::OpenUrl(target)
                }
                KeyCode::Enter => {
                    let code = editor.take_for_submit().unwrap_or_default();
                    if code.trim().is_empty() {
                        validation = Some("인증 코드를 입력하세요.".to_owned());
                        self.pending = Some(PendingInteraction::ProviderOAuthCode {
                            provider_id,
                            provider_name,
                            method,
                            url,
                            instructions,
                            editor,
                            validation,
                        });
                        Action::None
                    } else {
                        Action::CompleteProviderOAuth {
                            provider_id,
                            provider_name,
                            method,
                            code,
                        }
                    }
                }
                KeyCode::Esc => Action::None,
                KeyCode::Backspace if ctrl => {
                    editor.delete_word_left();
                    self.pending = Some(PendingInteraction::ProviderOAuthCode {
                        provider_id,
                        provider_name,
                        method,
                        url,
                        instructions,
                        editor,
                        validation: None,
                    });
                    Action::None
                }
                KeyCode::Backspace => {
                    editor.backspace();
                    self.pending = Some(PendingInteraction::ProviderOAuthCode {
                        provider_id,
                        provider_name,
                        method,
                        url,
                        instructions,
                        editor,
                        validation: None,
                    });
                    Action::None
                }
                KeyCode::Char(ch) if !ctrl => {
                    editor.insert(ch);
                    self.pending = Some(PendingInteraction::ProviderOAuthCode {
                        provider_id,
                        provider_name,
                        method,
                        url,
                        instructions,
                        editor,
                        validation: None,
                    });
                    Action::None
                }
                _ => {
                    self.pending = Some(PendingInteraction::ProviderOAuthCode {
                        provider_id,
                        provider_name,
                        method,
                        url,
                        instructions,
                        editor,
                        validation,
                    });
                    Action::None
                }
            },
            PendingInteraction::ProviderOAuthWaiting {
                provider_name,
                url,
                instructions,
            } => {
                let action = if key.code == KeyCode::Char('o') && !ctrl && !alt {
                    Action::OpenUrl(url.clone())
                } else {
                    Action::None
                };
                self.pending = Some(PendingInteraction::ProviderOAuthWaiting {
                    provider_name,
                    url,
                    instructions,
                });
                action
            }
            PendingInteraction::Approval {
                id,
                title,
                detail,
                mut selected,
                once,
                session,
                session_label,
                decline,
            } => match key.code {
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    self.pending = Some(PendingInteraction::Approval {
                        id,
                        title,
                        detail,
                        selected,
                        once,
                        session,
                        session_label,
                        decline,
                    });
                    Action::None
                }
                KeyCode::Down => {
                    let last = 1 + usize::from(session.is_some());
                    selected = (selected + 1).min(last);
                    self.pending = Some(PendingInteraction::Approval {
                        id,
                        title,
                        detail,
                        selected,
                        once,
                        session,
                        session_label,
                        decline,
                    });
                    Action::None
                }
                KeyCode::Enter if selected == 0 => Action::RpcResponse { id, result: once },
                KeyCode::Enter if session.is_some() && selected == 1 => Action::RpcResponse {
                    id,
                    result: session.expect("checked"),
                },
                KeyCode::Enter => Action::RpcResponse {
                    id,
                    result: decline,
                },
                _ => {
                    self.pending = Some(PendingInteraction::Approval {
                        id,
                        title,
                        detail,
                        selected,
                        once,
                        session,
                        session_label,
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
                if user_input_text_focused(question, selected) {
                    match key.code {
                        KeyCode::Enter => {
                            let Some(answer) = editor.take_for_submit() else {
                                // Enter can reach the app while Windows Terminal
                                // is still committing an IME preedit. Never turn
                                // that transient empty editor into a sent answer.
                                self.pending = Some(PendingInteraction::UserInput {
                                    id,
                                    questions,
                                    current,
                                    selected,
                                    editor,
                                    answers,
                                });
                                return Action::None;
                            };
                            answers.insert(question.id.clone(), answer);
                            return next_question_or_reply(id, questions, current, answers, self);
                        }
                        // Claude Code makes `Other` an input option rather than a
                        // second mode. The arrows simply move focus away while the
                        // row keeps the text for the next visit.
                        KeyCode::Up if !question.options.is_empty() => {
                            selected = question.options.len() - 1;
                        }
                        KeyCode::Down if !question.options.is_empty() => {
                            selected = chat_instead_index(question);
                        }
                        KeyCode::Char('p') if ctrl && !question.options.is_empty() => {
                            selected = question.options.len() - 1;
                        }
                        KeyCode::Char('n') if ctrl && !question.options.is_empty() => {
                            selected = chat_instead_index(question);
                        }
                        KeyCode::Backspace if ctrl => editor.delete_word_left(),
                        KeyCode::Backspace => editor.backspace(),
                        KeyCode::Delete if ctrl => editor.delete_word_right(),
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
                    let chat_instead = chat_instead_index(question);
                    // The rows are numbered on screen, so their numbers answer the
                    // question: typing one moves to that row and takes it, rather
                    // than being swallowed as a character the picker has no use for.
                    let pressed = match key.code {
                        KeyCode::Char(ch) if !ctrl && !alt => match ch.to_digit(10) {
                            Some(digit) if digit >= 1 && digit as usize - 1 <= chat_instead => {
                                selected = digit as usize - 1;
                                KeyCode::Enter
                            }
                            _ => key.code,
                        },
                        code => code,
                    };
                    match pressed {
                        KeyCode::Up => selected = selected.saturating_sub(1),
                        KeyCode::Down => selected = (selected + 1).min(chat_instead),
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
                            // Chatting instead answers nothing: the tool gets
                            // what has been answered so far, exactly as Esc.
                            if selected == chat_instead {
                                return Action::RpcResponse {
                                    id,
                                    result: answers_response(&answers),
                                };
                            }
                            // Focusing the free-text row is enough. The next key is
                            // input immediately; no hidden Enter-only mode exists.
                        }
                        _ => {}
                    }
                }
                self.pending = Some(PendingInteraction::UserInput {
                    id,
                    questions,
                    current,
                    selected,
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
            } => match hotkey {
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
            } => match hotkey {
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
                let provider_models = self.current_provider_model_indices();
                let selected_position = provider_models
                    .iter()
                    .position(|index| index == model_index)
                    .unwrap_or(0);
                let window =
                    visible_window(Some(selected_position), provider_models.len(), PICKER_ROWS);
                let start = window.start;
                let mut lines = provider_models[window]
                    .iter()
                    .enumerate()
                    .map(|(offset, index)| {
                        let model = &self.models[*index];
                        OverlayLine {
                            text: format!("{}. {}", start + offset + 1, model.display_name),
                            selected: *index == *model_index,
                            muted: false,
                        }
                    })
                    .collect::<Vec<_>>();
                let slider = self
                    .models
                    .get(*model_index)
                    .filter(|model| !model.efforts.is_empty())
                    .map(|model| {
                        lines.push(OverlayLine {
                            text: String::new(),
                            selected: false,
                            muted: true,
                        });
                        effort_slider(model, *effort_index)
                    });
                let hint = if slider.is_some() {
                    "↑↓ model  ·  ←→ effort  ·  Enter to continue  ·  Esc to cancel"
                } else {
                    "↑↓ model  ·  Enter to continue  ·  Esc to cancel"
                };
                Some(OverlayView {
                    closable: true,
                    title: "Model".to_owned(),
                    lines,
                    slider,
                    hint: hint.to_owned(),
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
                let summary = if effort.is_empty() {
                    model.display_name.clone()
                } else {
                    format!("{}  ·  {effort}", model.display_name)
                };
                let label_width = ModelScope::CHOICES
                    .iter()
                    .map(|scope| scope.label().len())
                    .max()
                    .unwrap_or_default();
                let mut lines = vec![
                    OverlayLine {
                        text: summary,
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
            PendingInteraction::SidePanelPicker { stage_index } => Some(OverlayView {
                closable: true,
                title: "Side panel".to_owned(),
                lines: Vec::new(),
                slider: Some(EffortSlider {
                    efforts: SidePanelStage::CHOICES
                        .iter()
                        .map(|stage| stage.label().to_owned())
                        .collect(),
                    selected: *stage_index,
                    detail: None,
                }),
                hint: "←→ to adjust  ·  Enter to continue  ·  Esc to cancel".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::SidePanelScope { stage, selected } => {
                let label_width = ModelScope::CHOICES
                    .iter()
                    .map(|scope| scope.label().len())
                    .max()
                    .unwrap_or_default();
                let mut lines = vec![
                    OverlayLine {
                        text: stage.label().to_owned(),
                        selected: false,
                        muted: true,
                    },
                    OverlayLine {
                        text: String::new(),
                        selected: false,
                        muted: true,
                    },
                ];
                lines.extend(ModelScope::CHOICES.iter().enumerate().map(|(index, scope)| {
                    let detail = match scope {
                        ModelScope::Session => "Keeps this session's own size",
                        ModelScope::Default => "Uses this size for new sessions",
                    };
                    OverlayLine {
                        text: format!(
                            "{}. {:<label_width$}  ·  {detail}",
                            index + 1,
                            scope.label()
                        ),
                        selected: index == *selected,
                        muted: false,
                    }
                }));
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
            PendingInteraction::RuntimePicker { selected } => Some(OverlayView {
                closable: true,
                title: "Provider".to_owned(),
                lines: Vec::new(),
                slider: Some(EffortSlider {
                    efforts: (0..RUNTIME_CHOICES.len())
                        .map(|index| self.runtime_step_label(index))
                        .collect(),
                    selected: *selected,
                    detail: None,
                }),
                hint: "←→ 이동  ·  Enter 사용  ·  Space 연결 전환  ·  Esc 닫기".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
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
                    detail: setting.detail(*selected),
                }),
                hint: "←→ to adjust  ·  Enter to confirm  ·  Esc to cancel".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::ClaudePermissionPicker { selected } => Some(OverlayView {
                closable: true,
                title: "Permission mode".to_owned(),
                lines: Vec::new(),
                slider: Some(EffortSlider {
                    efforts: ClaudePermissionMode::choices(
                        self.claude_auto_mode_available(),
                        self.bypass_permissions_allowed,
                    )
                    .iter()
                    .map(|mode| mode.picker_label().to_owned())
                    .collect(),
                    selected: *selected,
                    detail: None,
                }),
                hint: "←→ 이동  ·  Enter 적용  ·  Esc 닫기".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::ClaudeAutoModeConsent { selected } => Some(OverlayView {
                closable: false,
                title: "Enable auto mode?".to_owned(),
                lines: [
                    "Auto mode executes actions without permission prompts after a safety classifier reviews them.",
                    "It reduces prompts but does not guarantee safety. Use it only when you trust the task direction.",
                    "Yes, enable auto mode",
                    "No",
                    "No, don't ask again",
                ]
                .into_iter()
                .enumerate()
                .map(|(index, text)| OverlayLine {
                    text: text.to_owned(),
                    selected: index >= 2 && index - 2 == *selected,
                    muted: index < 2,
                })
                .collect(),
                slider: None,
                hint: "↑↓ select  ·  Enter confirm  ·  Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::ClaudePermissionsPanel {
                tab,
                selected,
                entries,
                denials,
                retry,
                rules_locked,
            } => {
                let lines = if let Some(behavior) = claude_permission_behavior(*tab) {
                    let mut lines = entries
                        .iter()
                        .filter(|entry| entry.behavior == behavior)
                        .enumerate()
                        .map(|(index, entry)| OverlayLine {
                            text: format!(
                                "{}\n{}",
                                entry.value,
                                claude_permission_source_label(&entry.source)
                            ),
                            selected: index == *selected,
                            muted: false,
                        })
                        .collect::<Vec<_>>();
                    if lines.is_empty() {
                        lines.push(OverlayLine {
                            text: format!("No {} rules", CLAUDE_PERMISSION_TABS[*tab].to_lowercase()),
                            selected: false,
                            muted: true,
                        });
                    }
                    lines
                } else {
                    let mut lines = denials
                        .iter()
                        .enumerate()
                        .map(|(index, denial)| OverlayLine {
                            text: match (retry == &Some(index), denial.reason.is_empty()) {
                                (true, true) => format!("↻ {}", denial.tool),
                                (true, false) => {
                                    format!("↻ {}\n{}", denial.tool, denial.reason)
                                }
                                (false, true) => denial.tool.clone(),
                                (false, false) => {
                                    format!("{}\n{}", denial.tool, denial.reason)
                                }
                            },
                            selected: index == *selected,
                            muted: false,
                        })
                        .collect::<Vec<_>>();
                    if lines.is_empty() {
                        lines.push(OverlayLine {
                            text: "No recent denials".to_owned(),
                            selected: false,
                            muted: true,
                        });
                    }
                    lines
                };
                Some(OverlayView {
                    closable: true,
                    title: format!("Permissions · {}", CLAUDE_PERMISSION_TABS[*tab]),
                    lines,
                    slider: None,
                    hint: if *tab == 4 {
                        if retry.is_some() {
                            "←→ category  ·  ↑↓ move  ·  R unmark  ·  Esc retry & close".to_owned()
                        } else {
                            "←→ category  ·  ↑↓ move  ·  R retry  ·  Esc close".to_owned()
                        }
                    } else if *rules_locked {
                        "←→ category  ·  ↑↓ move  ·  Managed rules only  ·  Esc close".to_owned()
                    } else {
                        "←→ category  ·  ↑↓ move  ·  A add  ·  D remove  ·  Esc close".to_owned()
                    },
                    style: OverlayStyle::CompactPanel,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
            PendingInteraction::ClaudePermissionScopePicker { behavior, selected } => {
                Some(OverlayView {
                    closable: true,
                    title: format!("Add {behavior} rule · Scope"),
                    lines: Vec::new(),
                    slider: Some(EffortSlider {
                        efforts: CLAUDE_PERMISSION_SCOPES
                            .iter()
                            .map(|(label, _)| (*label).to_owned())
                            .collect(),
                        selected: *selected,
                        detail: None,
                    }),
                    hint: "←→ move  ·  Enter select  ·  Esc back".to_owned(),
                    style: OverlayStyle::Picker,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
            PendingInteraction::ClaudePermissionRuleInput {
                behavior,
                destination,
                editor,
            } => Some(OverlayView {
                closable: true,
                title: format!(
                    "Add {behavior} rule · {}",
                    claude_permission_source_label(destination)
                ),
                lines: Vec::new(),
                slider: None,
                hint: "Enter save  ·  Esc back".to_owned(),
                style: OverlayStyle::Panel,
                input: Some(editor),
                input_label: if behavior == "directory" { "Directory" } else { "Rule" },
                input_placeholder: if behavior == "directory" {
                    "../shared"
                } else {
                    "Bash(npm test)"
                },
            }),
            PendingInteraction::SubagentTranscript { id, offset } => {
                let log = self.subagent_logs.get(id);
                let running = self.subagents.iter().find(|running| &running.id == id);
                let mut lines = log
                    .into_iter()
                    .flatten()
                    .skip(*offset)
                    .take(PICKER_ROWS)
                    .map(|line| OverlayLine {
                        text: line.text.clone(),
                        selected: false,
                        muted: line.muted,
                    })
                    .collect::<Vec<_>>();
                if lines.is_empty() {
                    lines.push(OverlayLine {
                        text: "아직 기록된 작업이 없습니다.".to_owned(),
                        selected: false,
                        muted: true,
                    });
                }
                Some(OverlayView {
                    closable: true,
                    title: match running {
                        Some(running) if running.description.is_empty() => running.name.clone(),
                        Some(running) => format!("{} · {}", running.name, running.description),
                        // The row is gone once the subagent finishes, but the
                        // record it left behind stays readable.
                        None => "Subagent · 완료됨".to_owned(),
                    },
                    lines,
                    slider: None,
                    hint: "↑↓ scroll  ·  Esc to return".to_owned(),
                    style: OverlayStyle::Panel,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
            PendingInteraction::VibeModePicker { selected, .. } => {
                let vibe = VibeMode::PICKER_CHOICES[*selected];
                Some(OverlayView {
                    closable: true,
                    title: "Vibe Mode".to_owned(),
                    lines: Vec::new(),
                    slider: Some(EffortSlider {
                        efforts: VibeMode::PICKER_CHOICES
                            .iter()
                            .map(|mode| mode.picker_label().to_owned())
                            .collect(),
                        selected: *selected,
                        detail: Some(vibe.picker_detail().to_owned()),
                    }),
                    hint: "←→ 이동  ·  Enter 적용  ·  Esc 취소".to_owned(),
                    style: OverlayStyle::Picker,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
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
            PendingInteraction::SkillsPicker {
                provider,
                selected,
                skills,
                errors,
                notice,
            } => {
                let window = visible_window(Some(*selected), skills.len(), SKILLS_PICKER_ROWS);
                let mut lines = skills[window.clone()]
                    .iter()
                    .enumerate()
                    .map(|(offset, skill)| OverlayLine {
                        text: skill_picker_row(skill, SKILL_NAME_MAX_COLUMNS),
                        selected: window.start + offset == *selected,
                        muted: false,
                    })
                    .collect::<Vec<_>>();
                if skills.is_empty() {
                    lines.push(OverlayLine {
                        text: errors
                            .first()
                            .map(|error| format!("오류 · {error}"))
                            .unwrap_or_else(|| "설치된 Skill이 없습니다.".to_owned()),
                        selected: false,
                        muted: true,
                    });
                }
                lines.resize_with(SKILLS_PICKER_ROWS, || OverlayLine {
                    text: String::new(),
                    selected: false,
                    muted: true,
                });
                let hint = if let Some(notice) = notice.as_deref() {
                    format!("상태 · {notice}  ·  이동 ↑↓  ·  전환 Space/Enter  ·  닫기 Esc")
                } else if !errors.is_empty() {
                    format!("오류 {}개  ·  이동 ↑↓  ·  전환 Space/Enter  ·  닫기 Esc", errors.len())
                } else {
                    format!(
                        "제공자 {} ←→  ·  이동 ↑↓  ·  전환 Space/Enter  ·  닫기 Esc",
                        if *provider == SkillProvider::Claude {
                            "Claude"
                        } else {
                            "Codex"
                        }
                    )
                };
                Some(OverlayView {
                    closable: true,
                    title: "Skills".to_owned(),
                    lines,
                    slider: None,
                    hint,
                    style: OverlayStyle::CompactPanel,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
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
                hint: "1-6 select   ↑↓ navigate   Enter apply   Esc cancel".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::SessionPicker(picker) => Some(picker.overlay_view()),
            PendingInteraction::McpPicker(picker) => Some(picker.overlay_view()),
            PendingInteraction::PluginPicker(picker) => Some(picker.overlay_view()),
            PendingInteraction::MarketplacePicker(picker) => Some(picker.overlay_view()),
            PendingInteraction::ProviderLoading => Some(OverlayView {
                title: "Connect OpenCode provider".to_owned(),
                lines: vec![OverlayLine {
                    text: "Provider 목록과 인증 방식을 불러오는 중…".to_owned(),
                    selected: true,
                    muted: false,
                }],
                slider: None,
                hint: "잠시 기다려 주세요.".to_owned(),
                closable: false,
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::ProviderPicker(picker) => Some(picker.overlay_view()),
            PendingInteraction::ProviderOAuthCode {
                provider_name,
                url,
                instructions,
                editor,
                validation,
                ..
            } => {
                let mut lines = vec![
                    OverlayLine {
                        text: instructions.clone(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: url.clone(),
                        selected: false,
                        muted: true,
                    },
                ];
                if let Some(validation) = validation {
                    lines.push(OverlayLine {
                        text: validation.clone(),
                        selected: false,
                        muted: false,
                    });
                }
                Some(OverlayView {
                    title: format!("{provider_name} · OAuth"),
                    lines,
                    slider: None,
                    hint: "O 브라우저 열기  Enter 코드 전송  Esc 취소".to_owned(),
                    closable: false,
                    style: OverlayStyle::Panel,
                    input: Some(editor),
                    input_label: "인증 코드",
                    input_placeholder: "브라우저에 표시된 코드를 입력…",
                })
            }
            PendingInteraction::ProviderOAuthWaiting {
                provider_name,
                url,
                instructions,
            } => Some(OverlayView {
                title: format!("{provider_name} · OAuth 연결 중"),
                lines: vec![
                    OverlayLine {
                        text: instructions.clone(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: url.clone(),
                        selected: false,
                        muted: true,
                    },
                    OverlayLine {
                        text: "브라우저 인증이 끝나면 자동으로 연결됩니다.".to_owned(),
                        selected: true,
                        muted: false,
                    },
                ],
                slider: None,
                hint: "O 브라우저 다시 열기".to_owned(),
                closable: false,
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::Approval {
                title,
                detail,
                selected,
                session,
                session_label,
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
                    text: "이번만 허용".to_owned(),
                    selected: *selected == 0,
                    muted: false,
                });
                if session.is_some() {
                    lines.push(OverlayLine {
                        text: session_label.clone(),
                        selected: *selected == 1,
                        muted: false,
                    });
                }
                lines.push(OverlayLine {
                    text: "거부".to_owned(),
                    selected: *selected == 1 + usize::from(session.is_some()),
                    muted: false,
                });
                Some(OverlayView {
                    closable: false,
                    title: title.clone(),
                    lines,
                    slider: None,
                    hint: "↑↓ 선택   Enter 확정".to_owned(),
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
                    hint: "↑↓ 선택   Enter 확정".to_owned(),
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
                editor,
                ..
            } => {
                let question = &questions[*current];
                let mut lines = vec![OverlayLine {
                    text: question.question.clone(),
                    selected: false,
                    muted: false,
                }];
                // `Other` is an input option, as in Claude Code: reaching the row
                // focuses its editor immediately, and leaving it keeps the typed
                // value visible instead of restoring the placeholder.
                let free_text_row = question.options.len();
                let text_focused = user_input_text_focused(question, *selected);
                if !question.options.is_empty() {
                    lines.extend(question.options.iter().enumerate().map(|(index, option)| {
                        OverlayLine {
                            text: format!("{}\n{}", option.label, option.description),
                            selected: index == *selected,
                            muted: false,
                        }
                    }));
                    if question.allow_other {
                        let typed = editor.text();
                        lines.push(OverlayLine {
                            text: if typed.is_empty() {
                                OTHER_ANSWER_LABEL.to_owned()
                            } else {
                                typed
                            },
                            selected: *selected == free_text_row,
                            muted: !text_focused && editor.is_empty(),
                        });
                    }
                    // The way out of the question, kept last so the renderer's
                    // rule lands between it and the answers.
                    lines.push(OverlayLine {
                        text: CHAT_INSTEAD_LABEL.to_owned(),
                        selected: *selected == chat_instead_index(question),
                        muted: false,
                    });
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
                    hint: if text_focused {
                        "Enter 전송 · Esc 취소".to_owned()
                    } else {
                        "Enter 선택 · ↑/↓ 이동 · Esc 취소".to_owned()
                    },
                    style: OverlayStyle::Question,
                    input: text_focused.then_some(editor),
                    input_label: "Answer",
                    input_placeholder: "",
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
            .filter(|command| crate::open_code::PROVIDER_ENABLED || command.name != "/connect")
            .filter(|command| {
                !self.selected_model_name().starts_with("claude:") || command.name != "/fast"
            })
            .filter(|command| {
                command.name != "/effort"
                    || self
                        .selected_model()
                        .is_some_and(|model| !model.efforts.is_empty())
            })
            .filter(|command| command.name.starts_with(&text))
            .collect()
    }

    fn slash_suggestion_views(&self) -> Vec<SuggestionView> {
        if self.suggestions_dismissed_text.as_deref() == Some(self.editor.text().as_str()) {
            return Vec::new();
        }
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
        if self.suggestions_dismissed_text.as_deref() == Some(text.as_str()) {
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
        self.suggestions_dismissed_text = None;
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

    /// A resumed session keeps the previous transcript visible and names the wait,
    /// so the transition never looks like an empty new conversation. A fresh
    /// session stays silent because its welcome screen is already complete.
    fn activity(&self) -> Option<String> {
        // An idle screen can go a long time between ticks, so the lifetime is
        // enforced on read as well: a faded warning must not outlive its arm.
        if let Some(notice) = self.live_activity_notice() {
            return Some(notice.to_owned());
        }
        if self.host_loading {
            return Some("Loading session..".to_owned());
        }
        // Compaction outranks the ordinary turn label: the runtime that runs it as
        // a turn would otherwise report a `Working` response the user never asked for.
        if let Some(started) = self.compacting_started_at {
            if self.turn_interrupted {
                return Some("X Interrupted".to_owned());
            }
            let elapsed_label = format_elapsed(started.elapsed().as_secs());
            // Providers expose the boundary, not numeric progress. The renderer
            // supplies an indeterminate bar while this elapsed clock is running.
            return Some(format!("Compacting.. ({elapsed_label})"));
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
            .map(|duration| format!("✧ Completed ({})", format_elapsed(duration.as_secs())))
    }

    /// `/compact` was accepted by the runtime: run the activity spinner until the
    /// compacted boundary arrives (or the turn that carries it ends).
    pub fn begin_compaction(&mut self) {
        self.compacting_started_at = Some(Instant::now());
    }

    pub fn end_compaction(&mut self) {
        self.compacting_started_at = None;
    }

    pub fn compacting(&self) -> bool {
        self.compacting_started_at.is_some()
    }

    fn live_activity_notice(&self) -> Option<&str> {
        self.activity_notice
            .as_ref()
            .filter(|(_, shown_at, ttl)| shown_at.elapsed() < *ttl)
            .map(|(notice, _, _)| notice.as_str())
    }

    fn activity_model(&self) -> Option<String> {
        if self.live_activity_notice().is_some()
            || (!self.busy && !self.compacting() && self.last_completed_duration.is_none())
        {
            return None;
        }
        // The activity label is UI chrome, so it tracks the model currently
        // selected in the composer immediately. Billing keeps using the active
        // turn model separately in `active_cost_model`.
        Some(self.selected_model_name().to_owned())
    }

    /// Activity text animation runs from the wall clock rather than counted ticks,
    /// so its pace stays stable even when frames are delayed.
    fn activity_phase(&self) -> f32 {
        let Some(started) = self.compacting_started_at.or(self.turn_started_at) else {
            return 0.0;
        };
        let position = started.elapsed().as_millis() % SHIMMER_PERIOD.as_millis();
        position as f32 / SHIMMER_PERIOD.as_millis() as f32
    }

    /// The compacting block deliberately moves more slowly than its label.
    fn compaction_progress_phase(&self) -> f32 {
        let Some(started) = self.compacting_started_at else {
            return 0.0;
        };
        let position = started.elapsed().as_millis() % COMPACTION_ACTIVITY_PERIOD.as_millis();
        position as f32 / COMPACTION_ACTIVITY_PERIOD.as_millis() as f32
    }

    fn plan_shimmer_phase(&self) -> Option<f32> {
        let started = self.plan_shimmer_started_at?;
        let elapsed = started.elapsed();
        (elapsed < PLAN_SHIMMER_DURATION)
            .then(|| elapsed.as_secs_f32() / PLAN_SHIMMER_DURATION.as_secs_f32())
    }

    fn response_collapse_view(&self) -> Option<(u64, f32)> {
        if self.vibe_mode != VibeMode::SuperVibe
            || self.response_display_mode != ResponseDisplayMode::Completed
        {
            return None;
        }
        let transition = self.response_collapse?;
        let progress = transition.started_at.elapsed().as_secs_f32()
            / RESPONSE_COLLAPSE_DURATION.as_secs_f32();
        if progress >= 1.0 {
            return None;
        }
        let progress = progress.clamp(0.0, 1.0);
        let eased = progress * progress * (3.0 - 2.0 * progress);
        Some((transition.group_id, 1.0 - eased))
    }

    pub fn response_collapse_animating(&self) -> bool {
        self.response_collapse_view().is_some()
    }

    fn status_line(&self) -> StatusLineView {
        let context = self.context_window.and_then(|window| {
            (window > 0).then(|| {
                format!(
                    "ctx: {}/{} ({}%)",
                    context_token_label(self.context_tokens),
                    context_token_label(window),
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
            effort: (self.status_line_settings.enabled(StatusLineField::Effort)
                && self
                    .selected_model()
                    .is_some_and(|model| !model.efforts.is_empty()))
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
            five_hour_remaining: self
                .status_line_settings
                .enabled(StatusLineField::FiveHour)
                .then(|| self.five_hour_remaining.clone())
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

    fn open_side_panel_scope(&mut self, stage: SidePanelStage) {
        self.pending = Some(PendingInteraction::SidePanelScope { stage, selected: 0 });
    }

    fn apply_side_panel_scope(&mut self, stage: SidePanelStage, scope: ModelScope) -> Action {
        self.set_side_panel_stage(stage);
        if scope == ModelScope::Default {
            Action::PersistSidePanelDefault(stage)
        } else {
            Action::None
        }
    }

    fn apply_model(&mut self, index: usize, effort: Option<&str>) {
        self.commit_welcome_card();
        // One conversation, one runtime. A thread exists only once a prompt has been
        // sent, and moving that thread onto another runtime left it named after the
        // first while its turns ran on the second — two names for one session, and
        // the notifications for it stopped reaching the screen.
        if !self.thread_pending() {
            let next = self
                .models
                .get(index)
                .map(|model| model_runtime(&model.model));
            let current = self
                .models
                .get(self.selected_model)
                .map(|model| model_runtime(&model.model));
            if let (Some(next), Some(current)) = (next, current)
                && next != current
            {
                self.push_notice(
                    BlockKind::Warning,
                    "Provider 고정됨",
                    format!(
                        "대화가 시작된 뒤에는 provider를 바꿀 수 없습니다. {next} 모델로 이야기하려면 /new로 새 대화를 여세요."
                    ),
                );
                return;
            }
        }
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
        self.normalize_claude_permission_mode_for_selected_model();
        self.committed.push(Block::new(
            BlockKind::ModelChange,
            "✓ Model changed",
            if selected_effort.is_empty() {
                format!("↳ {model_name}")
            } else {
                format!("↳ {model_name} · {selected_effort}")
            },
        ));
    }

    fn move_model_index(&self, model_index: usize, direction: i8) -> usize {
        let candidates = self.current_provider_model_indices();
        let position = candidates
            .iter()
            .position(|candidate| *candidate == model_index)
            .unwrap_or(0);
        let next = match direction {
            -1 => position.saturating_sub(1),
            1 => (position + 1).min(candidates.len().saturating_sub(1)),
            _ => position,
        };
        candidates.get(next).copied().unwrap_or(model_index)
    }

    fn move_selected_model(&mut self, direction: i8) {
        let next_index = self.move_model_index(self.selected_model, direction);
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
        if next_index != current_index
            && let Some(effort) = effort
        {
            self.selected_effort = effort;
            self.notice_setting_applies_to_next_request();
        }
    }

    fn notice_setting_applies_to_next_request(&mut self) {
        if self.busy {
            self.set_composer_notice("Applies to the next request".to_owned());
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

    #[cfg(test)]
    pub const fn response_display_mode(&self) -> ResponseDisplayMode {
        self.response_display_mode
    }

    fn set_response_display_mode(&mut self, mode: ResponseDisplayMode) {
        self.response_display_mode = mode;
        if mode == ResponseDisplayMode::All {
            self.response_collapse = None;
        }
    }

    pub fn cycle_vibe_mode(&mut self) -> (ShellDisplayMode, DiffDisplayMode) {
        self.apply_vibe_mode(self.vibe_mode.next());
        (self.shell_display_mode, self.diff_display_mode)
    }

    fn apply_vibe_mode(&mut self, vibe_mode: VibeMode) {
        self.vibe_mode = vibe_mode;
        match vibe_mode {
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

    /// Steps the panel to its next width, wrapping closed after the widest. The
    /// new stage is written against this session right away, so a resume reopens
    /// on the width this session was left on rather than another session's.
    pub fn cycle_side_panel(&mut self) -> SidePanelStage {
        let stage = self.side_panel_stage.next();
        self.set_side_panel_stage(stage);
        stage
    }

    fn set_side_panel_stage(&mut self, stage: SidePanelStage) {
        self.side_panel_stage = stage;
        let _ = write_session_side_panel_stage(&self.thread_id, self.side_panel_stage);
    }

    /// Restores the panel this session was last left showing. Called whenever a
    /// thread is bound, which is what makes `/resume` reopen at the same width.
    fn restore_session_side_panel(&mut self) {
        if self.thread_id.is_empty() {
            return;
        }
        // A session nobody opened the panel in starts closed instead of
        // inheriting whatever width the previous session was left on.
        self.side_panel_stage = read_session_side_panel_stage(&self.thread_id)
            .unwrap_or_else(read_default_side_panel_stage);
    }

    /// Records this session's vibe/response modes beside its thread id so a later
    /// resume reopens on them. Called after any change that persists a mode.
    pub fn persist_session_modes(&self) {
        let _ = write_session_modes(
            &self.thread_id,
            vec![
                (
                    "vibe_mode".to_owned(),
                    self.vibe_mode.config_value().to_owned(),
                ),
                (
                    "model_verbosity".to_owned(),
                    self.response_length.model_verbosity().to_owned(),
                ),
                (
                    "response_display_mode".to_owned(),
                    self.response_display_mode.config_value().to_owned(),
                ),
                (
                    "shell_display_mode".to_owned(),
                    self.shell_display_mode.config_value().to_owned(),
                ),
                (
                    "diff_display_mode".to_owned(),
                    self.diff_display_mode.config_value().to_owned(),
                ),
            ],
        );
    }

    /// Restores the vibe/response modes this session was last left on. A thread
    /// with nothing saved keeps the global defaults already loaded, which is how
    /// resuming an older session stays on the global latest value.
    fn restore_session_modes(&mut self) {
        let Some(modes) = read_session_modes(&self.thread_id) else {
            return;
        };
        for (key, value) in modes {
            match key.as_str() {
                "vibe_mode" => {
                    self.vibe_mode = match value.as_str() {
                        "super_vibe" => VibeMode::SuperVibe,
                        "normal" => VibeMode::Normal,
                        _ => VibeMode::Vibe,
                    };
                }
                "model_verbosity" => {
                    self.response_length = match value.as_str() {
                        "medium" => ResponseLength::Normal,
                        "high" => ResponseLength::Detailed,
                        _ => ResponseLength::Short,
                    };
                }
                "response_display_mode" => {
                    if let Some(mode) = ResponseDisplayMode::from_config_value(&value) {
                        self.response_display_mode = mode;
                    }
                }
                "shell_display_mode" => {
                    if let Some(mode) = ShellDisplayMode::from_config_value(&value) {
                        self.shell_display_mode = mode;
                    }
                }
                "diff_display_mode" => {
                    if let Some(mode) = DiffDisplayMode::from_config_value(&value) {
                        self.diff_display_mode = mode;
                    }
                }
                _ => {}
            }
        }
    }

    #[cfg(test)]
    pub fn side_panel_open(&self) -> bool {
        self.side_panel_stage != SidePanelStage::Closed
    }

    #[cfg(test)]
    pub fn side_panel_stage(&self) -> SidePanelStage {
        self.side_panel_stage
    }

    pub fn toggle_plan_summary(&mut self) {
        if let Some(summary) = &mut self.plan_summary {
            summary.expanded = !summary.expanded;
        }
    }

    pub fn toggle_side_panel_prompts(&mut self) {
        self.side_panel_prompts_expanded = !self.side_panel_prompts_expanded;
    }

    pub fn toggle_side_panel_mcp(&mut self, provider: &str) {
        if let Some(snapshot) = self.integration_snapshot_mut(match provider {
            "Claude" => ModelProvider::Claude,
            "Codex" => ModelProvider::Codex,
            _ => return,
        }) {
            snapshot.mcp_expanded = !snapshot.mcp_expanded;
        }
    }

    pub fn toggle_side_panel_plugins(&mut self, provider: &str) {
        if let Some(snapshot) = self.integration_snapshot_mut(match provider {
            "Claude" => ModelProvider::Claude,
            "Codex" => ModelProvider::Codex,
            _ => return,
        }) {
            snapshot.plugins_expanded = !snapshot.plugins_expanded;
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
                let provider_models = self.current_provider_model_indices();
                let selected_position = provider_models
                    .iter()
                    .position(|index| *index == model_index)
                    .unwrap_or(0);
                let start =
                    visible_window(Some(selected_position), provider_models.len(), PICKER_ROWS)
                        .start;
                let clicked = start + row;
                if let Some(clicked) = provider_models.get(clicked).copied() {
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
            Some(PendingInteraction::SidePanelScope { stage, selected }) => match row
                .checked_sub(MODEL_SCOPE_HEADER_ROWS)
                .and_then(|choice| ModelScope::CHOICES.get(choice))
            {
                Some(scope) => self.apply_side_panel_scope(stage, *scope),
                None => {
                    self.pending = Some(PendingInteraction::SidePanelScope { stage, selected });
                    Action::Tick(false)
                }
            },
            Some(PendingInteraction::ThemePicker { theme_index }) => {
                match ThemeKind::ALL.get(row) {
                    Some(theme) => self.apply_theme(*theme),
                    None => {
                        self.pending = Some(PendingInteraction::ThemePicker { theme_index });
                        Action::Tick(false)
                    }
                }
            }
            // A click picks the runtime; the connection switch stays on Space, so
            // a mis-aimed click never drops a provider.
            Some(PendingInteraction::RuntimePicker { .. }) if row < RUNTIME_CHOICES.len() => {
                self.apply_runtime_choice(row)
            }
            Some(PendingInteraction::SkillsPicker {
                provider,
                selected,
                skills,
                errors,
                notice,
            }) => {
                let window = visible_window(Some(selected), skills.len(), SKILLS_PICKER_ROWS);
                let clicked = window.start.checked_add(row);
                let skill = clicked
                    .filter(|index| *index < window.end)
                    .and_then(|index| skills.get(index))
                    .cloned();
                match skill {
                    Some(skill) => {
                        let enabled = !skill.enabled;
                        self.pending = Some(PendingInteraction::SkillsPicker {
                            provider,
                            selected,
                            skills,
                            errors,
                            notice,
                        });
                        self.apply_skill_enabled(
                            provider,
                            &skill.path,
                            skill.source.as_deref(),
                            enabled,
                            Some(format!("{} · 저장 중", skill.name)),
                        );
                        Action::SetSkillEnabled {
                            provider,
                            name: skill.name,
                            path: skill.path,
                            source: skill.source,
                            scope: skill.scope,
                            enabled,
                        }
                    }
                    None => {
                        self.pending = Some(PendingInteraction::SkillsPicker {
                            provider,
                            selected,
                            skills,
                            errors,
                            notice,
                        });
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
            Some(PendingInteraction::ClaudeAutoModeConsent { selected }) => {
                match row.checked_sub(2) {
                    Some(choice) if choice < 3 => self.apply_claude_auto_mode_consent(choice),
                    _ => {
                        self.pending = Some(PendingInteraction::ClaudeAutoModeConsent { selected });
                        Action::Tick(false)
                    }
                }
            }
            Some(PendingInteraction::SessionPicker(mut picker)) => match picker.click_row(row) {
                SessionPickerResult::Select(thread_id) => Action::ResumeThread(thread_id),
                SessionPickerResult::Cancel => Action::None,
                SessionPickerResult::None => {
                    self.pending = Some(PendingInteraction::SessionPicker(picker));
                    Action::Tick(false)
                }
            },
            Some(PendingInteraction::ClaudePermissionsPanel {
                tab,
                selected,
                entries,
                denials,
                retry,
                rules_locked,
            }) => {
                let count = if let Some(behavior) = claude_permission_behavior(tab) {
                    entries
                        .iter()
                        .filter(|entry| entry.behavior == behavior)
                        .count()
                } else {
                    denials.len()
                };
                self.pending = Some(PendingInteraction::ClaudePermissionsPanel {
                    tab,
                    selected: if row < count { row } else { selected },
                    entries,
                    denials,
                    retry,
                    rules_locked,
                });
                Action::None
            }
            // 승인 프롬프트의 선택지 행은 클릭한 선택으로 응답한다. 상세
            // 행(선택지 앞의 detail)은 클릭해도 답이 되지 않는다.
            Some(PendingInteraction::Approval {
                id,
                title,
                detail,
                selected,
                once,
                session,
                session_label,
                decline,
            }) => {
                let first = detail.len();
                let decline_row = first + 1 + usize::from(session.is_some());
                if row == first {
                    Action::RpcResponse { id, result: once }
                } else if session.is_some() && row == first + 1 {
                    Action::RpcResponse {
                        id,
                        result: session.expect("checked"),
                    }
                } else if row == decline_row {
                    Action::RpcResponse {
                        id,
                        result: decline,
                    }
                } else {
                    self.pending = Some(PendingInteraction::Approval {
                        id,
                        title,
                        detail,
                        selected,
                        once,
                        session,
                        session_label,
                        decline,
                    });
                    Action::Tick(false)
                }
            }
            Some(PendingInteraction::Confirm {
                title,
                detail,
                action,
            }) => {
                if row == detail.len() {
                    action.into_action()
                } else if row == detail.len() + 1 {
                    Action::None
                } else {
                    self.pending = Some(PendingInteraction::Confirm {
                        title,
                        detail,
                        action,
                    });
                    Action::Tick(false)
                }
            }
            Some(PendingInteraction::UserInput {
                id,
                questions,
                current,
                selected,
                editor,
                mut answers,
            }) => {
                // Row zero is the prompt itself; the answers start under it.
                let question = &questions[current];
                let clicked = row.checked_sub(1);
                let chat_instead = chat_instead_index(question);
                match clicked {
                    Some(clicked) if clicked < question.options.len() => {
                        let label = question.options[clicked].label.clone();
                        answers.insert(question.id.clone(), label);
                        next_question_or_reply(id, questions, current, answers, self)
                    }
                    Some(clicked) if clicked == chat_instead => Action::RpcResponse {
                        id,
                        result: answers_response(&answers),
                    },
                    Some(clicked) if clicked == question.options.len() && question.allow_other => {
                        self.pending = Some(PendingInteraction::UserInput {
                            id,
                            questions,
                            current,
                            selected: clicked,
                            editor,
                            answers,
                        });
                        Action::None
                    }
                    _ => {
                        self.pending = Some(PendingInteraction::UserInput {
                            id,
                            questions,
                            current,
                            selected,
                            editor,
                            answers,
                        });
                        Action::Tick(false)
                    }
                }
            }
            other => {
                self.pending = other;
                Action::Tick(false)
            }
        }
    }

    /// The `✕` on a panel the user opened themselves: closes it, exactly as Esc
    /// does. Only the panels that paint the mark can be shut this way, so a prompt
    /// the server is waiting on stays put whatever is clicked.
    /// Opens the transcript panel for the subagent shown on the clicked row. The
    /// panel starts at the newest lines, which is where the work is.
    pub fn open_subagent(&mut self, index: usize) -> Action {
        let Some(id) = self.subagents.get(index).map(|running| running.id.clone()) else {
            return Action::None;
        };
        let offset = self.subagent_log_max_offset(&id);
        self.pending = Some(PendingInteraction::SubagentTranscript { id, offset });
        Action::Tick(true)
    }

    fn subagent_log_max_offset(&self, id: &str) -> usize {
        self.subagent_logs
            .get(id)
            .map(|log| log.len().saturating_sub(PICKER_ROWS))
            .unwrap_or(0)
    }

    pub fn close_overlay(&mut self) -> Action {
        match self.pending.take() {
            Some(PendingInteraction::ClaudePermissionsPanel {
                retry: Some(index),
                denials,
                ..
            }) => denials
                .get(index)
                .map(|denial| Action::RetryClaudePermissionDenial {
                    tool: denial.tool.clone(),
                    input: denial.input.clone(),
                })
                .unwrap_or(Action::None),
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
            Some(PendingInteraction::SidePanelPicker { stage_index }) => {
                match SidePanelStage::CHOICES.get(step).copied() {
                    Some(stage) => {
                        self.open_side_panel_scope(stage);
                        Action::None
                    }
                    None => {
                        self.pending = Some(PendingInteraction::SidePanelPicker { stage_index });
                        Action::Tick(false)
                    }
                }
            }
            Some(PendingInteraction::VibeModePicker {
                selected,
                vibe,
                response,
                shell,
                diff,
            }) => {
                if let Some(mode) = VibeMode::PICKER_CHOICES.get(step).copied() {
                    self.apply_vibe_mode(mode);
                    Action::PersistVibeDisplayModes {
                        vibe: self.vibe_mode,
                        response: self.response_length,
                        shell: self.shell_display_mode,
                        diff: self.diff_display_mode,
                    }
                } else {
                    self.pending = Some(PendingInteraction::VibeModePicker {
                        selected,
                        vibe,
                        response,
                        shell,
                        diff,
                    });
                    Action::Tick(false)
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
            Some(PendingInteraction::ClaudePermissionPicker { selected }) => {
                if step
                    < ClaudePermissionMode::choices(
                        self.claude_auto_mode_available(),
                        self.bypass_permissions_allowed,
                    )
                    .len()
                {
                    self.apply_claude_permission_picker(step)
                } else {
                    self.pending = Some(PendingInteraction::ClaudePermissionPicker { selected });
                    Action::Tick(false)
                }
            }
            Some(PendingInteraction::ClaudePermissionScopePicker { behavior, selected }) => {
                if step < CLAUDE_PERMISSION_SCOPES.len() {
                    self.pending = Some(PendingInteraction::ClaudePermissionRuleInput {
                        behavior,
                        destination: CLAUDE_PERMISSION_SCOPES[step].1.to_owned(),
                        editor: Editor::default(),
                    });
                    Action::None
                } else {
                    self.pending = Some(PendingInteraction::ClaudePermissionScopePicker {
                        behavior,
                        selected,
                    });
                    Action::Tick(false)
                }
            }
            Some(PendingInteraction::RuntimePicker { .. }) if step < RUNTIME_CHOICES.len() => {
                self.apply_runtime_choice(step)
            }
            Some(PendingInteraction::ProviderPicker(mut picker)) => {
                match picker.select_step(step) {
                    ProviderPickerResult::None => {
                        self.pending = Some(PendingInteraction::ProviderPicker(picker));
                        Action::None
                    }
                    ProviderPickerResult::Cancel => Action::None,
                    ProviderPickerResult::Submit(request) => Action::SubmitProviderAuth(request),
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
            DisplaySetting::Response => {
                let mode = match selected {
                    0 => ResponseDisplayMode::All,
                    1 => ResponseDisplayMode::Completed,
                    _ => self.response_display_mode,
                };
                self.set_response_display_mode(mode);
                Action::PersistResponseDisplayMode(mode)
            }
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
            &welcome.provider,
            &welcome.plan,
            &welcome.cwd,
            &welcome.account,
            &welcome.credits,
        ));
        self.show_welcome = false;
    }

    fn welcome_view(&self) -> WelcomeView {
        WelcomeView {
            provider: if self.selected_model_name().starts_with("claude:") {
                "Claude".to_owned()
            } else if self.selected_model_name().starts_with("opencode:") {
                "OpenCode".to_owned()
            } else {
                "Codex".to_owned()
            },
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
        if self.completed_item_ids.contains(id) {
            return;
        }
        let Some(mut block) = active_item_block(&self.cwd, item) else {
            return;
        };
        if matches!(block.kind, BlockKind::Assistant) {
            // A provider can label the streaming item's phase late or not at all.
            // Treat every new assistant item as the next visible response boundary:
            // the first item has nothing to fold, and each later item replaces the
            // same prompt-hosted group with everything completed before it. This
            // makes the item's first streamed row its settled row even when a final
            // answer was announced as commentary at item start.
            self.collapse_progress_before_next_answer();
        }
        if matches!(block.kind, BlockKind::Assistant) && !block.body.is_empty() {
            self.turn_response_started = true;
        }
        let existing_batch = self
            .active
            .get(id)
            .and_then(|existing| existing.shell_batch.clone());
        let revision = self
            .active
            .get(id)
            .map(|existing| existing.revision.wrapping_add(1))
            .unwrap_or(0);
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
        // Text already waiting for its frame belongs to this item; re-announcing
        // the item must not discard it.
        let pace = self
            .active
            .remove(id)
            .map(|existing| existing.pace)
            .unwrap_or_default();
        self.active.insert(
            id.to_owned(),
            ActiveItem {
                block,
                shell_batch,
                revision,
                pace,
            },
        );
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
                block.adopt_assistant_phase(&active.block);
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
                self.turn_response_started |= !block.body.is_empty();
                self.last_assistant_markdown = Some(block.body.clone());
                self.turn_response_blocks.push(block.clone());
            }
            if is_context_compaction(&block) {
                if self.push_unique_operation(block.clone()) {
                    self.turn_response_blocks.push(block);
                }
                return;
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
        if matches!(kind, BlockKind::Assistant) && !self.active.contains_key(item_id) {
            // OpenCode can begin an assistant item with its first delta instead
            // of a separate item/started notification. Settle earlier responses
            // at that same boundary so every provider reaches the common layout.
            self.collapse_progress_before_next_answer();
        }
        if matches!(kind, BlockKind::Assistant) && !delta.is_empty() {
            self.turn_response_started = true;
        }
        let active = self.ensure_active(item_id, kind, title);
        active.pace.push(delta);
    }

    /// Reveal the share of held text that `elapsed` has earned, for every stream
    /// still holding some.
    pub fn drain_stream_text(&mut self, elapsed: Duration) -> StreamReveal {
        let mut reveal = StreamReveal::default();
        for active in self.active.values_mut() {
            if let Some(chunk) = active.pace.take(elapsed) {
                reveal.clusters += visible_cluster_count(&chunk);
                append_capped(&mut active.block.body, &chunk);
                active.revision = active.revision.wrapping_add(1);
            }
            reveal.backlog += visible_cluster_count(&active.pace.pending);
        }
        reveal.fade_changed = self.advance_stream_fade(reveal.clusters, elapsed);
        (reveal.final_frame_ready, reveal.released) =
            self.release_held_notifications(reveal.clusters > 0);
        reveal
    }

    /// Lengthen the settling tail by what just appeared and shorten it by the
    /// time that passed. A character therefore starts at the tail's far end and
    /// walks out of it, which is what turns a hard appearance into a rise.
    /// Returns whether the visible length changed.
    fn advance_stream_fade(&mut self, revealed: usize, elapsed: Duration) -> bool {
        let before = self.stream_fade_tail.round() as usize;
        let grown = self.stream_fade_tail + revealed as f32;
        let faded = grown - STREAM_FADE_SPEED * elapsed.as_secs_f32();
        self.stream_fade_tail = faded.clamp(0.0, STREAM_FADE_MAX_TAIL);
        before != self.stream_fade_tail.round() as usize
    }

    /// Held text must land before an item or turn is finalized; anything still
    /// waiting would otherwise be dropped when the block leaves `active`.
    fn flush_stream_text(&mut self) {
        for active in self.active.values_mut() {
            let Some(rest) = active.pace.flush() else {
                continue;
            };
            append_capped(&mut active.block.body, &rest);
            active.revision = active.revision.wrapping_add(1);
        }
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
        let active = self
            .active
            .get_mut(item_id)
            .expect("shell output is active");
        append_capped(&mut active.block.body, delta);
        active.revision = active.revision.wrapping_add(1);
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
                    revision: 0,
                    pace: TextPace::default(),
                },
            );
        }
        let active = self.active.get_mut(item_id).expect("active shell exists");
        active.shell_batch = Some(batch_id.clone());
        active.block.title = "Shell · command".to_owned();
        active.revision = active.revision.wrapping_add(1);

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
                    revision: 0,
                    pace: TextPace::default(),
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
                    self.turn_response_started |= !item.block.body.is_empty();
                    self.last_assistant_markdown = Some(item.block.body.clone());
                    self.turn_response_blocks.push(item.block.clone());
                }
                if is_context_compaction(&item.block) {
                    if self.push_unique_operation(item.block.clone()) {
                        self.turn_response_blocks.push(item.block);
                    }
                    continue;
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
        commit_user_input_answers(state, &questions, &answers);
        return Action::RpcResponse {
            id,
            result: answers_response(&answers),
        };
    }
    let next = current + 1;
    state.pending = Some(PendingInteraction::UserInput {
        id,
        questions,
        current: next,
        selected: 0,
        editor: Editor::default(),
        answers,
    });
    Action::None
}

/// Leave the answer in conversation history when the blocking question closes.
/// The RPC response alone reaches the model but otherwise leaves no visible proof
/// that Enter sent the text the user just typed.
fn commit_user_input_answers(
    state: &mut AppState,
    questions: &[Question],
    answers: &BTreeMap<String, String>,
) {
    let answered = questions
        .iter()
        .filter_map(|question| {
            let answer = answers.get(&question.id)?.trim();
            (!answer.is_empty()).then_some((question, answer))
        })
        .collect::<Vec<_>>();
    if answered.is_empty() {
        return;
    }
    let body = answered
        .into_iter()
        .map(|(question, answer)| {
            let question = question
                .question
                .trim()
                .trim_end_matches([':', '：', '?', '？']);
            let answer = strip_recommendation_mark(answer);
            format!("{question}:\n  ↳ {answer}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    state.commit_welcome_card();
    // 답변 블록도 사용자가 직접 보낸 메시지와 같은 모델 색을 써야 하므로 제목에
    // 모델 이름을 넣는다. "You"로 두면 렌더러가 모델을 못 알아보고 기본 강조색으로
    // 떨어진다.
    let title = state.selected_model_name().to_owned();
    state
        .committed
        .push(Block::new(BlockKind::User, title, body));
}

/// `(권장)`은 고르기 전에만 쓸모 있는 안내라, 확정된 답변 기록에서는 떼어 낸다.
/// 같은 뜻으로 붙는 `(추천)`도 함께 떼어 낸다.
fn strip_recommendation_mark(answer: &str) -> &str {
    let trimmed = answer.trim_end();
    for mark in ["(권장)", "(추천)", "(Recommended)"] {
        if let Some(rest) = trimmed.strip_suffix(mark) {
            let rest = rest.trim_end();
            if !rest.is_empty() {
                return rest;
            }
        }
    }
    trimmed
}

/// The two rows a question carries beyond its own options.
const OTHER_ANSWER_LABEL: &str = "직접 입력";
const CHAT_INSTEAD_LABEL: &str = "이 내용으로 대화하기";

/// Claude Code treats its automatic `Other` row as the editor itself: focus is
/// the input mode. A question with no choices remains a plain text prompt.
fn user_input_text_focused(question: &Question, selected: usize) -> bool {
    question.options.is_empty() || question.allow_other && selected == question.options.len()
}

/// Where the row that leaves the question sits: after the options and after the
/// free-text row when the question offers one.
fn chat_instead_index(question: &Question) -> usize {
    question.options.len() + usize::from(question.allow_other)
}

fn answers_response(answers: &BTreeMap<String, String>) -> Value {
    let mut map = Map::new();
    for (id, answer) in answers {
        map.insert(id.clone(), json!({ "answers": [answer] }));
    }
    json!({ "answers": map })
}

fn parse_questions(params: &Value) -> Vec<Question> {
    let decoded = (params.get("encoding").and_then(Value::as_str) == Some("base64-json"))
        .then(|| params.get("payload").and_then(Value::as_str))
        .flatten()
        .and_then(|payload| BASE64.decode(payload).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let params = decoded.as_ref().unwrap_or(params);
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

fn assistant_phase(item: &Value) -> AssistantPhase {
    match item.get("phase").and_then(Value::as_str) {
        Some("commentary") => AssistantPhase::Commentary,
        Some("final_answer") | Some("finalAnswer") => AssistantPhase::FinalAnswer,
        _ => AssistantPhase::Unknown,
    }
}

fn active_item_block(cwd: &str, item: &Value) -> Option<Block> {
    match item.get("type")?.as_str()? {
        "agentMessage" => Some(
            Block::new(
                BlockKind::Assistant,
                item.get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex"),
                item.get("text").and_then(Value::as_str).unwrap_or_default(),
            )
            .with_assistant_phase(assistant_phase(item)),
        ),
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

/// The plan as it reaches the transcript, in either shape it arrives in: its own
/// kind from a runtime that reports plans directly, or a reasoning block titled
/// `Plan` from one that streams them as thinking. A resume replays whichever the
/// session recorded, which is how the steps came back after Super Vibe had
/// already taken the dock panel off the frame.
fn is_plan_block(block: &Block) -> bool {
    matches!(block.kind, BlockKind::Plan)
        || (matches!(block.kind, BlockKind::Reasoning) && block.title == "Plan")
}

fn is_context_compaction(block: &Block) -> bool {
    matches!(block.kind, BlockKind::System) && block.title == "Context compacted"
}

/// Operations whose repeated cards add no information. The body participates
/// in the signature, so two calls to the same tool with different results stay
/// visible; Web Search includes its query in the title for the same reason.
fn operation_signature(block: &Block) -> Option<String> {
    if is_context_compaction(block) {
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

/// The title a replayed prompt carries until a model is found for its turn.
/// The marker colour is read off that title, so a prompt left with this one
/// would lose the model colour it had while it was sent.
const UNKNOWN_PROMPT_MODEL: &str = "You";

fn user_message_text(item: &Value) -> Option<String> {
    (item.get("type")?.as_str()? == "userMessage")
        .then(|| {
            item.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|content| {
                    (content.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| content.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|body| !body.is_empty())
}

fn completed_item_block(cwd: &str, item: &Value) -> Option<Block> {
    match item.get("type")?.as_str()? {
        "userMessage" => user_message_text(item)
            .map(|body| Block::new(BlockKind::User, UNKNOWN_PROMPT_MODEL, body)),
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
    let prompt_model = turn
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| {
            items.iter().find_map(|item| {
                (item.get("type").and_then(Value::as_str) == Some("userMessage"))
                    .then(|| item.get("model").and_then(Value::as_str))
                    .flatten()
            })
        })
        .or_else(|| {
            rollout.and_then(|rollout| {
                turn.get("startedAt")
                    .and_then(Value::as_i64)
                    .and_then(|started_at| rollout.model_for_turn(started_at))
            })
        });
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
        if let Some(mut block) = completed_item_block(cwd, item) {
            if matches!(block.kind, BlockKind::User)
                && let Some(model) = prompt_model
            {
                block.title = model.to_owned();
            }
            rows.push((last_ts.clone(), order, block));
            order += 1;
        }
    }
    // The rollout is a fallback, not a second source: when the turn's own items
    // already carry the shell runs or the thinking, replaying the rollout copy
    // put a card on screen that the live turn never showed. Only a turn whose
    // items are missing that kind still needs the rollout to supply it.
    let item_kinds = items
        .iter()
        .filter_map(|item| item.get("type").and_then(Value::as_str))
        .collect::<HashSet<_>>();
    let items_carry_shell = item_kinds.contains("commandExecution");
    let items_carry_reasoning = item_kinds.contains("reasoning");
    let mut emitted_shell_group = false;
    for event in &events {
        let block = match &event.kind {
            RolloutKind::Reasoning { .. } if items_carry_reasoning => continue,
            RolloutKind::Exec { .. } if items_carry_shell => continue,
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
    let blocks = normalized_turn_blocks(rows.into_iter().map(|(_, _, block)| block).collect());
    let successful = turn
        .get("status")
        .and_then(Value::as_str)
        .is_none_or(|status| status == "completed")
        && turn.get("error").is_none_or(Value::is_null);
    if successful {
        group_turn_response(blocks)
    } else {
        blocks
    }
}

fn progress_groups_for_prompt_ids(progress: Vec<Block>, prompt_ids: &[u64]) -> Vec<Block> {
    let mut groups: Vec<(usize, Vec<Block>)> = Vec::new();
    for block in progress {
        let boundary = prompt_ids.partition_point(|prompt_id| *prompt_id < block.id());
        if groups
            .last()
            .is_none_or(|(current, _)| *current != boundary)
        {
            groups.push((boundary, Vec::new()));
        }
        groups.last_mut().expect("progress group").1.push(block);
    }
    groups
        .into_iter()
        .map(|(_, children)| Block::progress_group(children))
        .collect()
}

fn group_turn_response(blocks: Vec<Block>) -> Vec<Block> {
    let assistant_indices = blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| matches!(block.kind, BlockKind::Assistant).then_some(index))
        .collect::<Vec<_>>();
    if assistant_indices.is_empty() {
        return blocks;
    }
    let final_index = assistant_indices
        .iter()
        .copied()
        .rfind(|&index| blocks[index].assistant_phase() == AssistantPhase::FinalAnswer)
        .unwrap_or(*assistant_indices.last().expect("assistant block"));
    let fold_indices = (0..final_index)
        .filter(|&index| {
            is_context_compaction(&blocks[index])
                || (matches!(blocks[index].kind, BlockKind::Assistant)
                    && !blocks[index].body.trim().is_empty())
        })
        .collect::<Vec<_>>();
    if fold_indices.is_empty() {
        return blocks;
    }
    let mut groups: Vec<(Option<usize>, Vec<Block>)> = Vec::new();
    for &index in &fold_indices {
        let prompt = blocks[..index]
            .iter()
            .rposition(|block| matches!(block.kind, BlockKind::User));
        if groups.last().is_none_or(|(current, _)| *current != prompt) {
            groups.push((prompt, Vec::new()));
        }
        groups
            .last_mut()
            .expect("progress group")
            .1
            .push(blocks[index].clone());
    }
    let progress_ids = fold_indices
        .iter()
        .map(|&index| blocks[index].id())
        .collect::<HashSet<_>>();
    let mut replacements = groups
        .into_iter()
        .map(|(_, children)| {
            let group = Block::progress_group(children);
            (group.id(), group)
        })
        .collect::<HashMap<_, _>>();
    let mut grouped = Vec::with_capacity(blocks.len());
    for block in blocks {
        if let Some(group) = replacements.remove(&block.id()) {
            grouped.push(group);
        } else if !progress_ids.contains(&block.id()) {
            grouped.push(block);
        }
    }
    grouped
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

/// `thread/resume` returns durable `plan` items with the turn history. Use the
/// newest one only when the local rollout is unavailable, because sessions
/// resumed on another machine do not have a rollout to parse.
fn plan_snapshot_from_history(turns: &[Value]) -> Option<PlanSnapshot> {
    turns
        .iter()
        .rev()
        .filter_map(|turn| turn.get("items").and_then(Value::as_array))
        .flat_map(|items| items.iter().rev())
        .find_map(|item| {
            (item.get("type").and_then(Value::as_str) == Some("plan")).then_some(item)?;
            plan_snapshot_from_steps(item).or_else(|| {
                item.get("text")
                    .and_then(Value::as_str)
                    .and_then(plan_snapshot_from_text)
            })
        })
}

/// A plan item that carries its own measured step times. The text form cannot
/// say how long a step took, so a plan restored from it alone totals to zero;
/// this reads the times the provider recorded beside the text.
fn plan_snapshot_from_steps(item: &Value) -> Option<PlanSnapshot> {
    let steps = item
        .get("steps")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|step| {
            let text = step.get("step").and_then(Value::as_str)?.trim();
            (!text.is_empty()).then_some(crate::rollout::PlanStepSnapshot {
                text: text.to_owned(),
                status: step
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("pending")
                    .to_owned(),
                elapsed_ms: step.get("elapsedMs").and_then(Value::as_u64),
            })
        })
        .collect::<Vec<_>>();
    (!steps.is_empty()).then_some(PlanSnapshot {
        explanation: None,
        steps,
    })
}

fn plan_snapshot_from_text(text: &str) -> Option<PlanSnapshot> {
    let steps = text
        .lines()
        .filter_map(plan_step_from_text)
        .collect::<Vec<_>>();
    (!steps.is_empty()).then_some(PlanSnapshot {
        explanation: None,
        steps,
    })
}

fn plan_step_from_text(line: &str) -> Option<crate::rollout::PlanStepSnapshot> {
    let line = line.trim();
    let (status, text) =
        if let Some(text) = line.strip_prefix("✓ ").or_else(|| line.strip_prefix("✔ ")) {
            ("completed", text)
        } else if let Some(text) = line.strip_prefix("▸ ") {
            ("in_progress", text)
        } else if let Some(text) = line.strip_prefix("□ ") {
            ("pending", text)
        } else if let Some(text) = line
            .strip_prefix("- [x] ")
            .or_else(|| line.strip_prefix("* [x] "))
        {
            ("completed", text)
        } else if let Some(text) = line
            .strip_prefix("- [~] ")
            .or_else(|| line.strip_prefix("* [~] "))
        {
            ("in_progress", text)
        } else {
            let text = line
                .strip_prefix("- [ ] ")
                .or_else(|| line.strip_prefix("* [ ] "))?;
            ("pending", text)
        };
    (!text.trim().is_empty()).then_some(crate::rollout::PlanStepSnapshot {
        text: text.trim().to_owned(),
        status: status.to_owned(),
        elapsed_ms: None,
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

fn approval_session_choice(params: &Value, response: Value) -> (Option<Value>, String) {
    if params
        .get("claudePermission")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let label = params
            .get("persistentApprovalLabel")
            .and_then(Value::as_str)
            .filter(|label| !label.is_empty())
            .unwrap_or("")
            .to_owned();
        return ((!label.is_empty()).then_some(response), label);
    }
    (Some(response), "세션 동안 허용".to_owned())
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
        detail: None,
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

/// Which runtime a model runs on. Only the family matters: a conversation is
/// pinned to the runtime that owns its session.
fn model_runtime(model: &str) -> &'static str {
    if crate::claude::is_claude_model(model) {
        "Claude"
    } else if crate::open_code::is_open_code_model(model) {
        "OpenCode"
    } else {
        "Codex"
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

/// Threads started before the verbatim prefix was stripped still echo back
/// `\\?\C:\...`, so every folder the server reports is normalised on the way in.
fn plain_folder(cwd: String) -> String {
    crate::plain_windows_path(PathBuf::from(cwd))
        .to_string_lossy()
        .into_owned()
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

fn read_codex_usage() -> (Option<u8>, Option<u8>, Option<u64>) {
    let Some(path) = env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("DevezCode")
            .join("codex-usage.json")
    }) else {
        return (None, None, None);
    };
    let Some(root) = fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str::<Value>(&json).ok())
    else {
        return (None, None, None);
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

fn parse_codex_usage(root: &Value) -> (Option<u8>, Option<u8>, Option<u64>) {
    (
        usage_percent(root, "five_hour"),
        usage_percent(root, "weekly"),
        root.get("five_hour")
            .and_then(|window| window.get("resets_at"))
            .and_then(reset_timestamp),
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

fn read_vibe_config_value(key: &str) -> Option<String> {
    // Tests build their state through the same constructor, so a maintainer whose
    // own settings.toml carries a different display mode would fail the tests that
    // assume the shipped defaults. Under test the file is not consulted at all.
    if cfg!(test) {
        return None;
    }
    vibe_settings_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|config| config_value(&config, key))
        .or_else(|| {
            codex_home()
                .and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
                .and_then(|config| config_value(&config, key))
        })
}

/// Where each session's own panel stage is kept. The stage is a per-session
/// preference, so it rides beside the settings file rather than inside it.
fn read_default_side_panel_stage() -> SidePanelStage {
    read_vibe_config_value("side_panel_stage")
        .map(|value| SidePanelStage::from_config_value(&value))
        .unwrap_or_default()
}

fn side_panel_stages_path() -> Option<PathBuf> {
    vibe_settings_path().and_then(|path| Some(path.parent()?.join("side-panel-stages.json")))
}

/// How many sessions the file remembers. Old entries are dropped oldest-first so
/// the file cannot grow without bound as sessions accumulate.
const SIDE_PANEL_STAGE_HISTORY: usize = 200;

fn read_side_panel_stages() -> Vec<(String, String)> {
    let Some(path) = side_panel_stages_path() else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<(String, String)>>(&text).unwrap_or_default()
}

/// The stage `thread_id` was last left on. A session nobody has opened the panel
/// in starts closed rather than inheriting another session's width.
fn read_session_side_panel_stage(thread_id: &str) -> Option<SidePanelStage> {
    if thread_id.is_empty() {
        return None;
    }
    read_side_panel_stages()
        .into_iter()
        .find(|(id, _)| id == thread_id)
        .map(|(_, stage)| SidePanelStage::from_config_value(&stage))
}

/// Moves `thread_id` to the newest end of the list with its current stage, then
/// trims the oldest entries past the history limit.
fn upsert_side_panel_stage(
    mut stages: Vec<(String, String)>,
    thread_id: &str,
    stage: SidePanelStage,
) -> Vec<(String, String)> {
    stages.retain(|(id, _)| id != thread_id);
    stages.push((thread_id.to_owned(), stage.config_value().to_owned()));
    if stages.len() > SIDE_PANEL_STAGE_HISTORY {
        let excess = stages.len() - SIDE_PANEL_STAGE_HISTORY;
        stages.drain(0..excess);
    }
    stages
}

fn write_session_side_panel_stage(thread_id: &str, stage: SidePanelStage) -> std::io::Result<()> {
    if thread_id.is_empty() {
        return Ok(());
    }
    let path = side_panel_stages_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Devez Vibe 설정 경로를 찾을 수 없습니다.",
        )
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stages = upsert_side_panel_stage(read_side_panel_stages(), thread_id, stage);
    let text = serde_json::to_string(&stages)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    fs::write(path, text)
}

/// Per-session vibe/response modes, kept beside the settings file the way the
/// side-panel stages are: a resume of the same thread reopens on the modes it
/// was left on rather than whatever the global default has drifted to since.
fn session_modes_path() -> Option<PathBuf> {
    vibe_settings_path().and_then(|path| Some(path.parent()?.join("session-modes.json")))
}

/// How many sessions the file remembers, trimmed oldest-first like the side
/// panel's own history so the file cannot grow without bound.
const SESSION_MODE_HISTORY: usize = 200;

fn read_session_modes_file() -> Vec<(String, Vec<(String, String)>)> {
    let Some(path) = session_modes_path() else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// The vibe/response modes `thread_id` was last left on, or `None` when this
/// session has none saved — in which case the caller keeps the global defaults.
fn read_session_modes(thread_id: &str) -> Option<Vec<(String, String)>> {
    if thread_id.is_empty() {
        return None;
    }
    read_session_modes_file()
        .into_iter()
        .find(|(id, _)| id == thread_id)
        .map(|(_, modes)| modes)
}

fn write_session_modes(thread_id: &str, modes: Vec<(String, String)>) -> std::io::Result<()> {
    if thread_id.is_empty() {
        return Ok(());
    }
    let path = session_modes_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Devez Vibe 설정 경로를 찾을 수 없습니다.",
        )
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let sessions = upsert_session_modes(read_session_modes_file(), thread_id, modes);
    let text = serde_json::to_string(&sessions)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    fs::write(path, text)
}

/// Moves `thread_id` to the newest end with its current modes, then trims the
/// oldest entries past the history limit — the side panel's rule, reused.
fn upsert_session_modes(
    mut sessions: Vec<(String, Vec<(String, String)>)>,
    thread_id: &str,
    modes: Vec<(String, String)>,
) -> Vec<(String, Vec<(String, String)>)> {
    sessions.retain(|(id, _)| id != thread_id);
    sessions.push((thread_id.to_owned(), modes));
    if sessions.len() > SESSION_MODE_HISTORY {
        let excess = sessions.len() - SESSION_MODE_HISTORY;
        sessions.drain(0..excess);
    }
    sessions
}

/// Whether dvz may dial the Codex app-server. Unset means no: a runtime is
/// connected only once the user has chosen it in `/provider`.
pub(crate) fn codex_provider_enabled() -> bool {
    provider_connected(CODEX_PROVIDER_KEY)
}

pub(crate) fn claude_provider_enabled() -> bool {
    provider_connected(CLAUDE_PROVIDER_KEY)
}

fn provider_connected(key: &str) -> bool {
    read_vibe_config_value(key)
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(false)
}

fn read_status_line_settings() -> StatusLineSettings {
    let mut settings = StatusLineSettings::default();
    for field in StatusLineField::ALL {
        if let Some(enabled) =
            read_vibe_config_value(field.config_key()).and_then(|value| value.parse::<bool>().ok())
        {
            settings.0[field.index()] = enabled;
        }
    }
    settings
}

fn config_value(config: &str, key: &str) -> Option<String> {
    config.lines().find_map(|line| {
        let (found, value) = line.split('#').next()?.split_once('=')?;
        (found.trim() == key).then(|| value.trim().trim_matches(['\"', '\'']).to_ascii_lowercase())
    })
}

fn vibe_settings_path() -> Option<PathBuf> {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("DevezVibe").join("settings.toml"))
        .or_else(|| {
            env::var_os("HOME").map(PathBuf::from).map(|home| {
                home.join(".config")
                    .join("devez-vibe")
                    .join("settings.toml")
            })
        })
}

pub(crate) fn write_vibe_config_value(key: &str, value: &str) -> std::io::Result<()> {
    let path = vibe_settings_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Devez Vibe 설정 경로를 찾을 수 없습니다.",
        )
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existing = fs::read_to_string(&path).unwrap_or_default();
    fs::write(path, upsert_vibe_config_value(&existing, key, value))
}

fn upsert_vibe_config_value(existing: &str, key: &str, value: &str) -> String {
    let replacement = format!(
        "{key} = {}",
        serde_json::to_string(value).unwrap_or_default()
    );
    let mut found = false;
    let mut lines = existing
        .lines()
        .map(|line| {
            let matches = line
                .split('#')
                .next()
                .and_then(|line| line.split_once('='))
                .is_some_and(|(found, _)| found.trim() == key);
            if matches {
                found = true;
                replacement.clone()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>();
    if !found {
        lines.push(replacement);
    }
    format!("{}\n", lines.join("\n"))
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

    /// The mark guides the choice and means nothing once one is made, in either
    /// wording the model reaches for.
    #[test]
    fn a_chosen_answer_drops_its_recommendation_mark() {
        assert_eq!(strip_recommendation_mark("바로 배포 (권장)"), "바로 배포");
        assert_eq!(strip_recommendation_mark("바로 배포 (추천)"), "바로 배포");
        assert_eq!(
            strip_recommendation_mark("바로 배포 (Recommended)"),
            "바로 배포"
        );
        // Nothing else to show, so the mark is the answer and stays whole.
        assert_eq!(strip_recommendation_mark("(추천)"), "(추천)");
    }

    #[test]
    fn encoded_questions_preserve_backslashes_and_korean_text() {
        let payload = BASE64.encode(
            json!({
                "questions": [{
                    "id": "q0",
                    "header": "정리 범위",
                    "question": "경로 C:\\\\temp와 \\u{ac00}를 유지할까요?",
                    "options": [{ "label": "유지", "description": "그대로 둡니다." }],
                    "isOther": true
                }]
            })
            .to_string(),
        );
        let questions = parse_questions(&json!({
            "encoding": "base64-json",
            "payload": payload
        }));

        assert_eq!(questions.len(), 1);
        assert_eq!(
            questions[0].question,
            "경로 C:\\\\temp와 \\u{ac00}를 유지할까요?"
        );
        assert_eq!(questions[0].options[0].label, "유지");
    }

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

    /// The stage cycles closed → small → medium → large → closed, and its saved
    /// config value must round-trip through the same parser a restored session
    /// reads it back with — otherwise a saved "large" would reopen closed.
    #[test]
    fn side_panel_stage_cycles_and_round_trips_its_config_value() {
        let mut stage = SidePanelStage::Closed;
        let mut widths = Vec::new();
        for _ in 0..4 {
            stage = stage.next();
            widths.push(stage.width());
        }
        assert_eq!(
            widths,
            vec![
                Some(SIDE_PANEL_WIDTHS[0]),
                Some(SIDE_PANEL_WIDTHS[1]),
                Some(SIDE_PANEL_WIDTHS[2]),
                None,
            ]
        );

        for stage in [
            SidePanelStage::Closed,
            SidePanelStage::Small,
            SidePanelStage::Medium,
            SidePanelStage::Large,
        ] {
            assert_eq!(
                SidePanelStage::from_config_value(stage.config_value()),
                stage
            );
        }
        assert_eq!(
            SidePanelStage::from_config_value("garbage"),
            SidePanelStage::Closed
        );
    }

    /// Each session keeps its own stage, so reopening one restores that session's
    /// width without another session's stage leaking into it.
    #[test]
    fn side_panel_stages_are_kept_per_session_and_bounded() {
        let stages = upsert_side_panel_stage(Vec::new(), "session-a", SidePanelStage::Large);
        let stages = upsert_side_panel_stage(stages, "session-b", SidePanelStage::Small);
        // Re-saving a session moves it to the newest end rather than duplicating.
        let stages = upsert_side_panel_stage(stages, "session-a", SidePanelStage::Medium);

        assert_eq!(
            stages,
            vec![
                ("session-b".to_owned(), "small".to_owned()),
                ("session-a".to_owned(), "medium".to_owned()),
            ]
        );

        let mut many = Vec::new();
        for index in 0..SIDE_PANEL_STAGE_HISTORY + 5 {
            many =
                upsert_side_panel_stage(many, &format!("session-{index}"), SidePanelStage::Small);
        }
        assert_eq!(many.len(), SIDE_PANEL_STAGE_HISTORY);
        assert_eq!(many[0].0, "session-5");
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
            supports_auto_mode: slug.starts_with("claude:"),
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

    fn two_runtime_state(thread_id: &str) -> AppState {
        AppState::new(
            thread_id.to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![
                test_model("gpt-5.6-sol", "GPT-5.6 Sol", true),
                test_model("claude:claude-opus-5", "Claude Opus", false),
            ],
            "gpt-5.6-sol",
            Some("high"),
        )
    }

    #[test]
    fn a_conversation_that_has_started_keeps_the_runtime_it_started_on() {
        let mut state = two_runtime_state("thread");

        state.apply_model(1, None);

        assert_eq!(state.selected_model_name(), "gpt-5.6-sol");
        assert!(
            state
                .committed
                .iter()
                .any(|block| block.title == "Provider 고정됨")
        );
    }

    #[test]
    fn a_conversation_with_no_session_yet_can_still_change_runtime() {
        let mut state = two_runtime_state("");

        state.apply_model(1, None);

        assert_eq!(state.selected_model_name(), "claude:claude-opus-5");
    }

    #[test]
    fn integration_snapshots_stay_with_the_provider_that_was_queried() {
        let mut state = AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![
                test_model("gpt-5.6-sol", "GPT-5.6 Sol", true),
                test_model("claude:claude-opus-5", "Claude Opus", false),
            ],
            "gpt-5.6-sol",
            Some("high"),
        );
        state.claude_provider_enabled = true;
        state.codex_provider_enabled = true;
        state.update_mcp_servers_for_model(
            &json!({
                "data": [{
                    "name": "claude-mcp",
                    "status": "connected",
                    "tools": {},
                    "authStatus": "unsupported"
                }]
            }),
            "claude:claude-opus-5",
        );
        state.update_plugins_for_model(
            &json!({
                "marketplaces": [{
                    "name": "codex",
                    "plugins": [{
                        "id": "codex-plugin@codex",
                        "name": "codex-plugin",
                        "installed": true,
                        "enabled": true,
                        "availability": "AVAILABLE",
                        "interface": { "displayName": "Codex Plugin" }
                    }]
                }]
            }),
            "gpt-5.6-sol",
        );

        let views = state.side_panel_integration_views();
        let claude = views
            .iter()
            .find(|view| view.provider == "Claude")
            .expect("Claude snapshot");
        let codex = views
            .iter()
            .find(|view| view.provider == "Codex")
            .expect("Codex snapshot");

        assert_eq!(
            claude.mcp.as_ref().expect("Claude MCP")[0].name,
            "claude-mcp"
        );
        assert!(claude.plugins.is_none());
        assert_eq!(
            codex.plugins.as_ref().expect("Codex plugin")[0].name,
            "Codex Plugin"
        );
        assert!(codex.mcp.is_none());
    }

    #[test]
    fn mcp_startup_failures_do_not_cross_provider_snapshots() {
        let mut state = AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![
                test_model("gpt-5.6-sol", "GPT-5.6 Sol", true),
                test_model("claude:claude-opus-5", "Claude Opus", false),
            ],
            "gpt-5.6-sol",
            Some("high"),
        );
        state.update_mcp_servers_for_model(&json!({ "data": [] }), "gpt-5.6-sol");
        state.note_mcp_failure("codex-failed".to_owned(), Some("spawn failed".to_owned()));
        state.update_mcp_servers_for_model(&json!({ "data": [] }), "claude:claude-opus-5");

        assert!(
            state
                .codex_integrations
                .mcp
                .as_ref()
                .is_some_and(|items| items.iter().any(|item| item.name == "codex-failed"))
        );
        assert!(
            state
                .claude_integrations
                .mcp
                .as_ref()
                .is_some_and(Vec::is_empty)
        );
    }

    #[test]
    fn free_text_answer_shows_what_was_typed() {
        let mut state = test_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "header": "대상 아이콘",
                    "options": [
                        { "label": "첫째", "description": "설명" },
                        { "label": "둘째", "description": "설명" }
                    ]
                }]
            }),
        );

        // Claude Code's `Other` row becomes the editor as soon as focus reaches it.
        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Char('가'), KeyModifiers::NONE));

        let overlay = state.overlay_view().expect("overlay");
        assert_eq!(overlay.input.map(Editor::text).as_deref(), Some("가"));
        assert_eq!(overlay.input_placeholder, "");
        // The answer is typed among the options, so they stay on screen and the
        // row being typed on is the one marked.
        assert!(
            overlay
                .lines
                .iter()
                .any(|line| line.text.starts_with("첫째")),
            "the options left the screen while the answer was typed"
        );
        assert_eq!(
            overlay
                .lines
                .iter()
                .position(|line| line.selected)
                .map(|row| overlay.lines[row].text.clone()),
            Some("가".to_owned())
        );
        // An arrow walks back out to the options it was typed among.
        state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        let overlay = state.overlay_view().expect("overlay");
        assert!(overlay.input.is_none());
        assert!(
            overlay
                .lines
                .iter()
                .any(|line| line.selected && line.text.starts_with("둘째"))
        );
        assert!(
            overlay.lines.iter().any(|line| line.text == "가"),
            "leaving Other restored its placeholder over the typed value"
        );
    }

    #[test]
    fn typing_only_reaches_the_focused_other_row() {
        let mut state = test_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": [
                        { "label": "첫째", "description": "설명" },
                        { "label": "둘째", "description": "설명" }
                    ]
                }]
            }),
        );

        state.handle_key(KeyEvent::new(KeyCode::Char('가'), KeyModifiers::NONE));
        let overlay = state.overlay_view().expect("overlay");
        assert!(overlay.input.is_none(), "a normal option became an editor");
        assert!(
            overlay
                .lines
                .iter()
                .any(|line| line.text == OTHER_ANSWER_LABEL)
        );

        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let overlay = state.overlay_view().expect("overlay");
        assert_eq!(overlay.input.map(Editor::text).as_deref(), Some(""));
    }

    #[test]
    fn clicking_an_option_leaves_the_other_editor_and_answers_that_option() {
        let mut state = test_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": [
                        { "label": "첫째", "description": "설명" },
                        { "label": "둘째", "description": "설명" }
                    ]
                }]
            }),
        );
        state.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Char('직'), KeyModifiers::NONE));

        let action = state.click_overlay_row(1);

        assert!(matches!(
            action,
            Action::RpcResponse { ref result, .. } if result.to_string().contains("첫째")
        ));
    }

    #[test]
    fn a_sent_free_text_answer_stays_visible_as_the_user_message() {
        let mut state = test_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": [
                        { "label": "첫째", "description": "설명" },
                        { "label": "둘째", "description": "설명" }
                    ]
                }]
            }),
        );

        state.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        for ch in "직접 보낸 답".chars() {
            state.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let action = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(
            matches!(
                action,
                Action::RpcResponse { ref result, .. }
                    if result.to_string().contains("직접 보낸 답")
            ),
            "the typed answer did not reach the server response"
        );
        let sent = state.committed.last().expect("sent answer history");
        assert!(matches!(sent.kind, BlockKind::User));
        assert_eq!(sent.body, "어느 것인가요:\n  ↳ 직접 보낸 답");
    }

    #[test]
    fn multiple_question_answers_keep_each_answer_under_its_question() {
        let mut state = test_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [
                    {
                        "id": "q1",
                        "question": "첫 질문인가요?",
                        "options": [{ "label": "첫 답", "description": "설명" }]
                    },
                    {
                        "id": "q2",
                        "question": "둘째 질문:",
                        "options": [
                            { "label": "아니오", "description": "설명" },
                            { "label": "둘째 답", "description": "설명" }
                        ]
                    }
                ]
            }),
        );

        assert!(matches!(
            state.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
            Action::None
        ));
        assert!(matches!(
            state.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
            Action::RpcResponse { .. }
        ));

        let sent = state.committed.last().expect("sent answer history");
        assert_eq!(
            sent.body,
            "첫 질문인가요:\n  ↳ 첫 답\n\n둘째 질문:\n  ↳ 둘째 답"
        );
    }

    #[test]
    fn enter_does_not_send_an_empty_inline_answer_during_ime_commit() {
        let mut state = test_state();
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
        state.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));

        assert!(matches!(
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        ));
        let overlay = state.overlay_view().expect("the question should stay open");
        assert_eq!(overlay.input.map(Editor::text).as_deref(), Some(""));
        assert!(
            !state
                .committed
                .iter()
                .any(|block| matches!(block.kind, BlockKind::User)),
            "an empty answer was shown as sent"
        );
    }

    /// The rows are numbered on screen, so the number is what a reader reaches
    /// for. Before, the digit fell through and the question sat unanswered while
    /// everything typed after it went nowhere.
    #[test]
    fn a_question_row_number_takes_that_row() {
        let question = json!({
            "questions": [{
                "id": "q1",
                "question": "어느 것인가요?",
                "options": [
                    { "label": "첫째", "description": "설명" },
                    { "label": "둘째", "description": "설명" }
                ]
            }]
        });

        let mut state = test_state();
        state.begin_server_request(json!(1), "item/tool/requestUserInput", &question);
        // Row 3 is the free-text row: two options, then 직접 입력.
        state.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Char('가'), KeyModifiers::NONE));
        let overlay = state.overlay_view().expect("overlay");
        assert_eq!(overlay.input.map(Editor::text).as_deref(), Some("가"));

        let mut state = test_state();
        state.begin_server_request(json!(1), "item/tool/requestUserInput", &question);
        let answered = state.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(
            matches!(
                answered,
                Action::RpcResponse { ref result, .. }
                    if result.to_string().contains("둘째")
            ),
            "row 2 did not answer with its own option"
        );
    }

    #[test]
    fn an_inline_question_answer_pauses_repaints_that_erase_ime_preedit() {
        let question = json!({
            "questions": [{
                "id": "q1",
                "question": "어느 것인가요?",
                "options": [
                    { "label": "첫째", "description": "설명" },
                    { "label": "둘째", "description": "설명" },
                    { "label": "셋째", "description": "설명" }
                ]
            }]
        });
        let mut state = test_state();
        state.busy = true;
        state.begin_server_request(json!(1), "item/tool/requestUserInput", &question);

        assert!(
            state.render_tick().redraw,
            "the running turn normally animates"
        );
        state.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        let composing = state.render_tick();
        assert!(
            !composing.redraw,
            "a tick would erase the terminal's IME preedit"
        );
        assert!(!composing.animation_only);

        state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(
            state.render_tick().redraw,
            "animation resumes after leaving the inline answer"
        );
    }

    #[test]
    fn host_loading_marks_the_devezcode_spinner_busy_without_a_turn() {
        let mut state = test_state();

        assert!(!state.host_turn_busy());
        assert!(!state.host_loading());
        state.set_host_loading(true);
        assert!(!state.host_turn_busy());
        assert!(state.host_loading());
        assert_eq!(state.view().activity.as_deref(), Some("Loading session.."));

        state.set_host_loading(false);
        assert!(!state.host_loading());
        assert_eq!(state.view().activity, None);
    }

    #[test]
    fn response_command_shows_descriptions_and_applies_both_modes() {
        let mut state = test_state();
        state.set_response_display_mode(ResponseDisplayMode::Completed);

        assert!(matches!(state.run_slash_command("/Response"), Action::None));
        let overlay = state.overlay_view().expect("Response picker");
        let slider = overlay.slider.expect("Response choices");
        assert_eq!(overlay.title, "Response");
        assert_eq!(slider.efforts, ["All", "Completed"]);
        assert_eq!(slider.selected, 1);
        assert_eq!(
            slider.detail.as_deref(),
            Some(
                "Super Vibe 모드에서만 동작합니다. 완료되면 마지막 답변만 남기고 이전 응답을 접습니다."
            )
        );
        assert_eq!(
            DisplaySetting::Response.detail(0).as_deref(),
            Some("Super Vibe 모드에서만 동작합니다. 모든 진행 응답을 항상 표시합니다.")
        );

        let action = state.click_effort_step(0);
        assert!(matches!(
            action,
            Action::PersistResponseDisplayMode(ResponseDisplayMode::All)
        ));
        assert_eq!(state.response_display_mode(), ResponseDisplayMode::All);

        let action = state.run_slash_command("/response Completed");
        assert!(matches!(
            action,
            Action::PersistResponseDisplayMode(ResponseDisplayMode::Completed)
        ));
        assert_eq!(
            state.response_display_mode(),
            ResponseDisplayMode::Completed
        );
    }

    #[test]
    fn all_response_mode_disables_super_vibe_progress_folding() {
        let mut state = test_state();
        while state.vibe_mode() != VibeMode::SuperVibe {
            state.cycle_vibe_mode();
        }

        state.set_response_display_mode(ResponseDisplayMode::Completed);
        assert!(state.view().fold_progress_groups);

        state.set_response_display_mode(ResponseDisplayMode::All);
        assert!(!state.view().fold_progress_groups);
        assert!(state.response_collapse_view().is_none());
    }

    /// The panel is the session's task view — the steps a turn is working
    /// through, live. Super Vibe keeps it: what that preset drops is the plan
    /// replayed into the transcript as a block, not the panel itself.
    #[test]
    fn super_vibe_keeps_the_plan_panel_on_the_frame() {
        let mut state = test_state();
        state.plan_summary = Some(PlanSummary {
            explanation: None,
            steps: vec![PlanStep {
                text: "1. 원인 확인".to_owned(),
                status: PlanStepStatus::InProgress,
                started_at: None,
                elapsed: None,
            }],
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        });
        while state.vibe_mode() != VibeMode::SuperVibe {
            state.cycle_vibe_mode();
        }

        assert!(state.view().plan_summary.is_some());
        assert!(state.animation_view().plan_summary.is_some());
        assert!(state.provider_handoff_plan().is_some());

        state.cycle_vibe_mode();
        assert!(state.view().plan_summary.is_some());
    }

    /// A resume replays the recorded plan as a transcript block, so hiding the
    /// dock panel alone left the steps coming back on every `/resume`. Both
    /// shapes the plan arrives in are dropped, and only under Super Vibe.
    #[test]
    fn super_vibe_drops_a_replayed_plan_block_from_the_transcript() {
        let mut state = test_state();
        // The welcome card would otherwise ride along at the head of the drain.
        state.show_welcome = false;
        let replayed = vec![
            Block::new(
                BlockKind::Reasoning,
                "Plan",
                "1. 빌드 복구 후 테스트 재실행",
            ),
            Block::new(BlockKind::Plan, "작업 단계", "2. 지침 검증"),
            Block::new(BlockKind::Assistant, "Codex", "본문은 남는다"),
        ];
        // A drain empties the queue, so each check starts from the same replay.
        let titles = |state: &mut AppState| {
            state.committed = replayed.clone();
            state
                .drain_committed()
                .iter()
                .map(|block| block.title.clone())
                .collect::<Vec<_>>()
        };
        while state.vibe_mode() != VibeMode::SuperVibe {
            state.cycle_vibe_mode();
        }

        assert_eq!(titles(&mut state), ["Codex"]);

        while state.vibe_mode() != VibeMode::Vibe {
            state.cycle_vibe_mode();
        }
        assert_eq!(titles(&mut state), ["Plan", "작업 단계", "Codex"]);
    }

    #[test]
    /// The starting mode comes from the config on disk, so the preset is cycled
    /// to rather than assumed.
    fn vibe_mode_preset_collapses_shell_and_diff_output() {
        let mut state = test_state();
        while state.vibe_mode() != VibeMode::Vibe {
            state.cycle_vibe_mode();
        }

        assert_eq!(state.vibe_mode_label(), "Vibe: On");
        assert_eq!(state.response_length_label(), "Short");
        assert_eq!(state.shell_display_mode(), ShellDisplayMode::Collapse);
        assert_eq!(state.diff_display_mode(), DiffDisplayMode::Collapse);
    }

    #[test]
    fn slash_display_setting_returns_vibe_mode_to_its_plain_preset() {
        let mut state = test_state();

        state.run_slash_command("/shell hide");

        assert_eq!(state.vibe_mode_label(), "Vibe: On");
        assert_eq!(state.shell_display_mode(), ShellDisplayMode::Hide);
    }

    #[test]
    fn vibe_mode_picker_previews_changes_and_escape_restores_them() {
        let mut state = test_state();
        while state.vibe_mode() != VibeMode::Vibe {
            state.cycle_vibe_mode();
        }
        state.run_slash_command("/vibemode");

        let overlay = state.overlay_view().expect("vibe picker");
        let slider = overlay.slider.expect("vibe choices");
        assert_eq!(slider.efforts, ["Off", "On", "Super Vibe"]);
        assert_eq!(slider.selected, 1);
        assert_eq!(
            slider.detail.as_deref(),
            Some("Diff와 명령어를 압축해서 표시합니다.")
        );

        state.handle_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(state.vibe_mode(), VibeMode::SuperVibe);
        assert_eq!(state.shell_display_mode(), ShellDisplayMode::Hide);
        assert_eq!(state.diff_display_mode(), DiffDisplayMode::Hide);
        assert_eq!(
            state
                .overlay_view()
                .and_then(|overlay| overlay.slider)
                .and_then(|slider| slider.detail),
            Some("Diff와 명령어 등을 모두 숨깁니다.".to_owned())
        );

        state.handle_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(state.response_length_label(), "Short");
        assert_eq!(state.vibe_mode_label(), "Vibe: On");
        assert_eq!(state.shell_display_mode(), ShellDisplayMode::Collapse);
        assert_eq!(state.diff_display_mode(), DiffDisplayMode::Collapse);
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
    fn esc_closes_slash_suggestions_before_interrupting_an_active_turn() {
        let mut state = test_state();
        state.busy = true;
        state.turn_started_at = Some(Instant::now());
        state.editor.set_text("/model");

        assert!(!state.view().suggestions.is_empty());
        assert!(matches!(
            state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Action::None
        ));
        assert!(state.view().suggestions.is_empty());
        assert!(!state.pending_interrupt);
        assert!(!state.turn_interrupted);
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
            .map(|live| live.block.title.as_str())
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

        state.ensure_active("dynamic-tool", BlockKind::Tool, "Tool · lookup");
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
        assert_eq!(live[0].block.title, "Web search · rust async");

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
        assert_eq!(
            state
                .plan_summary
                .as_ref()
                .map(|summary| summary.steps.len()),
            Some(1)
        );
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

    /// The prompt marker is coloured from the model named on the block, so a
    /// replayed prompt that carries no model of its own has to inherit the one
    /// the thread reopened on rather than staying on the neutral placeholder.
    #[test]
    fn a_replayed_prompt_without_a_model_takes_the_resumed_one() {
        let mut state = test_state();
        let thread = json!({
            "turns": [{
                "id": "turn-1",
                "startedAt": 1_784_992_108_i64,
                "items": [{
                    "type": "userMessage",
                    "id": "item-1",
                    "content": [{ "type": "text", "text": "질문" }]
                }]
            }]
        });

        state.load_history(&thread, None);

        let prompt = state
            .committed
            .iter()
            .find(|block| matches!(block.kind, BlockKind::User))
            .expect("replayed prompt");
        assert_eq!(prompt.title, "gpt-5.6-sol");
    }

    #[test]
    fn resumed_codex_and_claude_prompts_replace_composer_history() {
        for model in ["gpt-5.6-sol", "claude:claude-sonnet-5"] {
            let mut state = test_state();
            state.editor.set_text("prompt from another session");
            state.editor.take_for_submit();
            state.editor.set_text("draft while resuming");
            let thread = json!({
                "turns": [
                    {
                        "id": "turn-1",
                        "model": model,
                        "items": [{
                            "type": "userMessage",
                            "id": "user-1",
                            "content": [{ "type": "text", "text": "first resumed prompt" }]
                        }]
                    },
                    {
                        "id": "turn-2",
                        "model": model,
                        "items": [
                            {
                                "type": "userMessage",
                                "id": "user-2",
                                "content": [
                                    { "type": "text", "text": "second" },
                                    { "type": "image", "url": "ignored" },
                                    { "type": "text", "text": "resumed prompt" }
                                ]
                            },
                            { "type": "agentMessage", "id": "agent-2", "text": "done" }
                        ]
                    }
                ]
            });

            state.load_history(&thread, None);

            state.editor.history_previous();
            assert_eq!(state.editor.text(), "second\nresumed prompt", "{model}");
            assert_eq!(state.editor.history_position(), Some((2, 2)), "{model}");
            state.editor.history_previous();
            assert_eq!(state.editor.text(), "first resumed prompt", "{model}");
            state.editor.history_next();
            state.editor.history_next();
            assert_eq!(state.editor.text(), "draft while resuming", "{model}");
        }
    }

    /// A resumed session writes its first `turn_context` when the next turn
    /// runs, so every replayed turn predates it. Falling back to the earliest
    /// record keeps those prompts on a model colour.
    #[test]
    fn a_turn_older_than_every_context_takes_the_earliest_model() {
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:08:33.387Z","type":"turn_context","payload":{"model":"gpt-5.6-terra"}}"#,
        );

        assert_eq!(rollout.model_for_turn(0), Some("gpt-5.6-terra"));
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
        assert_eq!(titles[0], "+1 Response");
        assert_eq!(titles[1], "Shell · 1 command · completed · 1.6s");
        assert_eq!(titles[3], "Codex");
        assert!(matches!(state.committed[0].kind, BlockKind::ProgressGroup));
        assert_eq!(state.committed[0].children().len(), 1);
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

    /// The turn's own items already hold the shell run and the thinking, so the
    /// rollout's copies would be a second card for work shown once while live.
    #[test]
    fn resumed_turn_skips_rollout_copies_of_items_it_already_has() {
        let mut state = test_state();
        let thread = json!({
            "turns": [{
                "id": "turn-1",
                "startedAt": 1_784_992_108_i64,
                "completedAt": 1_784_992_379_i64,
                "items": [
                    { "type": "reasoning", "id": "think-1", "summary": ["살펴보는 중"] },
                    {
                        "type": "commandExecution",
                        "id": "exec-1",
                        "command": "cargo test",
                        "aggregatedOutput": "ok"
                    },
                    { "type": "agentMessage", "id": "item-1", "text": "고쳤습니다" }
                ]
            }]
        });
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:08:30.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"살펴보는 중"}]}}
{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"cargo test\"});","internal_chat_message_metadata_passthrough":{"turn_id":"turn-1"}}}
{"timestamp":"2026-07-25T15:08:38.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_one","output":[{"type":"input_text","text":"Wall time 1.6 seconds\n"},{"type":"input_text","text":"Exit code: 0\nOutput:\nok\n"}]}}
{"timestamp":"2026-07-25T15:09:58.000Z","type":"event_msg","payload":{"type":"agent_message","message":"고쳤습니다"}}"#,
        );

        state.load_history(&thread, Some(&rollout));

        let shells = state
            .committed
            .iter()
            .filter(|block| block.title.starts_with("Shell ·"))
            .count();
        let thinking = state
            .committed
            .iter()
            .filter(|block| is_thinking(block))
            .count();
        assert_eq!(shells, 1);
        assert_eq!(thinking, 1);
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
    fn a_background_subagent_survives_its_parent_turn_until_the_bridge_removes_it() {
        let mut state = test_state();

        state.handle_notification(
            "turn/subagents/updated",
            &json!({
                "subagents": [{
                    "id": "toolu_1",
                    "name": "Explore",
                    "description": "Find auth code",
                    "tool": "Grep(fn login)",
                }],
            }),
        );
        let running = state.view().subagents;

        assert_eq!(running.len(), 1);
        assert_eq!(running[0].name, "Explore");
        assert_eq!(running[0].tool, "Grep(fn login)");

        state.handle_notification("turn/completed", &json!({}));

        assert_eq!(state.view().subagents.len(), 1);

        state.handle_notification("turn/subagents/updated", &json!({ "subagents": [] }));

        assert!(state.view().subagents.is_empty());
    }

    #[test]
    fn a_subagent_that_keeps_running_keeps_its_start_instant() {
        let mut state = test_state();
        let update = |tool: &str| {
            json!({
                "subagents": [{ "id": "toolu_1", "name": "Explore", "description": "", "tool": tool }],
            })
        };

        state.handle_notification("turn/subagents/updated", &update("Grep"));
        let started = state.subagents[0].started_at;
        state.handle_notification("turn/subagents/updated", &update("Read"));

        assert_eq!(state.subagents[0].started_at, started);
        assert_eq!(state.subagents[0].tool, "Read");
    }

    fn state_with_a_running_subagent() -> AppState {
        let mut state = test_state();
        state.handle_notification(
            "turn/subagents/updated",
            &json!({
                "subagents": [{
                    "id": "toolu_1",
                    "name": "Explore",
                    "description": "Find auth code",
                    "tool": "Grep(fn login)",
                }],
            }),
        );
        state
    }

    fn subagent_line_notification(kind: &str, text: &str) -> Value {
        json!({
            "parentToolUseId": "toolu_1",
            "line": { "kind": kind, "text": text },
        })
    }

    #[test]
    fn subagent_lines_are_recorded_with_a_marker_for_each_kind() {
        let mut state = state_with_a_running_subagent();

        state.handle_notification(
            "turn/subagent/line",
            &subagent_line_notification("text", "auth 코드를 찾는 중"),
        );
        state.handle_notification(
            "turn/subagent/line",
            &subagent_line_notification("tool", "Grep(fn login)"),
        );
        state.handle_notification(
            "turn/subagent/line",
            &subagent_line_notification("result", "3 matches"),
        );
        state.handle_notification(
            "turn/subagent/line",
            &subagent_line_notification("text", "   "),
        );

        let log = &state.subagent_logs["toolu_1"];
        assert_eq!(
            log.iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["auth 코드를 찾는 중", "⏺ Grep(fn login)", "  ⎿ 3 matches"],
            "a blank line carries nothing and is left out"
        );
        assert_eq!(
            log.iter().map(|line| line.muted).collect::<Vec<_>>(),
            [false, true, true]
        );
    }

    #[test]
    fn clicking_a_subagent_row_opens_its_transcript_and_esc_returns_to_main() {
        let mut state = state_with_a_running_subagent();
        state.handle_notification(
            "turn/subagent/line",
            &subagent_line_notification("text", "찾는 중"),
        );

        state.open_subagent(0);
        let overlay = state.overlay_view().expect("subagent panel");

        assert_eq!(overlay.title, "Explore · Find auth code");
        assert_eq!(
            overlay
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["찾는 중"]
        );
        assert!(overlay.closable);

        state.close_overlay();

        assert!(state.overlay_view().is_none());
    }

    #[test]
    fn a_background_subagent_panel_survives_the_parent_turn() {
        let mut state = state_with_a_running_subagent();
        state.handle_notification(
            "turn/subagent/line",
            &subagent_line_notification("text", "찾는 중"),
        );
        state.open_subagent(0);

        state.handle_notification("turn/completed", &json!({}));

        assert!(state.overlay_view().is_some());
        assert_eq!(state.subagent_logs["toolu_1"].len(), 1);

        state.handle_notification("turn/subagents/updated", &json!({ "subagents": [] }));

        assert_eq!(
            state.overlay_view().map(|overlay| overlay.title),
            Some("Subagent · 완료됨".to_owned())
        );
    }

    #[test]
    fn an_idle_background_subagent_keeps_its_elapsed_row_redrawing() {
        let mut state = state_with_a_running_subagent();
        state.busy = false;
        state.subagents[0].started_at = Instant::now() - Duration::from_secs(1);

        let tick = state.render_tick();

        assert!(tick.redraw);
        assert!(!tick.animation_only);

        let same_second = state.render_tick();
        assert!(!same_second.redraw);
    }

    #[test]
    fn switching_sessions_clears_background_subagents_and_their_logs() {
        let mut state = state_with_a_running_subagent();
        state.handle_notification(
            "turn/subagent/line",
            &subagent_line_notification("text", "찾는 중"),
        );

        state.prepare_resume();

        assert!(state.subagents.is_empty());
        assert!(state.subagent_logs.is_empty());
    }

    #[test]
    fn clicking_a_row_that_is_already_gone_opens_nothing() {
        let mut state = test_state();

        state.open_subagent(0);

        assert!(state.overlay_view().is_none());
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
        assert_eq!(titles[0], "+1 Response");
        assert_eq!(state.committed[0].children().len(), 1);
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

        // The welcome card is committed on the first plan update; the plan itself
        // stays in the fixed panel instead of becoming a transcript card.
        assert!(
            state
                .committed
                .iter()
                .all(|block| matches!(block.kind, BlockKind::Welcome))
        );
        assert_eq!(
            state
                .plan_summary
                .as_ref()
                .map(|summary| summary.steps.len()),
            Some(3)
        );
        assert_eq!(
            state
                .plan_summary
                .as_ref()
                .and_then(|summary| summary.explanation.as_deref()),
            Some("범위를 확인했습니다.")
        );
        assert!(
            state
                .plan_summary
                .as_ref()
                .is_some_and(|summary| summary.expanded)
        );
        assert_eq!(
            state
                .plan_summary
                .as_ref()
                .expect("received plan")
                .steps
                .iter()
                .map(|step| step.text.as_str())
                .collect::<Vec<_>>(),
            vec!["1. 현재 구현 확인", "2. 표시 동작 구현", "3. 회귀 테스트"]
        );
        state.prepare_resume();
        assert!(state.plan_summary.is_none());
    }

    #[test]
    fn completed_plan_steps_preserve_observed_time_without_inventing_it() {
        let mut observed = test_state();
        observed.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "inProgress" }] }),
        );
        observed.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "completed" }] }),
        );
        let elapsed = observed.plan_summary.as_ref().unwrap().steps[0].elapsed;
        observed.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "completed" }] }),
        );
        assert!(elapsed.is_some());
        assert_eq!(
            observed.plan_summary.as_ref().unwrap().steps[0].elapsed,
            elapsed
        );

        let mut unobserved = test_state();
        unobserved.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "completed" }] }),
        );
        assert_eq!(
            unobserved.plan_summary.as_ref().unwrap().steps[0].elapsed,
            None
        );
    }

    #[test]
    fn completed_plan_does_not_reopen_when_the_next_turn_starts() {
        let mut state = test_state();
        state.set_turn_started("turn-one".to_owned());
        state.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "done", "status": "completed" }] }),
        );
        assert!(state.view().plan_active);

        state.handle_notification("turn/completed", &json!({}));
        state.set_turn_started("turn-two".to_owned());

        assert!(!state.view().plan_active);
        assert_eq!(
            state.plan_summary.as_ref().unwrap().steps[0].status,
            PlanStepStatus::Completed
        );

        state.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "next", "status": "inProgress" }] }),
        );
        assert!(state.view().plan_active);
    }

    #[test]
    fn plan_collapse_survives_next_prompt_and_plan_update() {
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

        assert!(
            state
                .plan_summary
                .as_ref()
                .is_some_and(|summary| !summary.expanded)
        );
    }

    #[test]
    fn resumed_plan_keeps_in_progress_steps() {
        let mut state = test_state();
        state.restore_plan_snapshot(&PlanSnapshot {
            explanation: None,
            steps: vec![
                crate::rollout::PlanStepSnapshot {
                    text: "완료 작업".to_owned(),
                    status: "completed".to_owned(),
                    elapsed_ms: Some(1_000),
                },
                crate::rollout::PlanStepSnapshot {
                    text: "진행 중이던 작업".to_owned(),
                    status: "in_progress".to_owned(),
                    elapsed_ms: Some(2_000),
                },
            ],
        });

        let steps = &state.plan_summary.expect("restored plan").steps;
        assert_eq!(steps[0].text, "1. 완료 작업");
        assert_eq!(steps[1].text, "2. 진행 중이던 작업");
        assert_eq!(steps[0].status, PlanStepStatus::Completed);
        assert_eq!(steps[1].status, PlanStepStatus::InProgress);
        assert_eq!(steps[1].elapsed, Some(Duration::from_secs(2)));
    }

    #[test]
    fn resumed_plan_item_restores_expanded_summary_without_local_rollout() {
        let mut state = test_state();
        let thread = json!({
            "turns": [{
                "items": [{
                    "type": "plan",
                    "text": "✔ 확인 완료\n▸ 수정 진행\n□ 검증 대기"
                }]
            }]
        });

        state.load_history(&thread, None);

        let summary = state.plan_summary.expect("restored plan summary");
        assert!(summary.expanded);
        assert_eq!(summary.steps.len(), 3);
        assert_eq!(summary.steps[0].status, PlanStepStatus::Completed);
        assert_eq!(summary.steps[1].status, PlanStepStatus::InProgress);
        assert_eq!(summary.steps[2].status, PlanStepStatus::Pending);
    }

    /// Without a local rollout the plan text is the only thing left, and it
    /// cannot say how long a step took — so the total under the card read zero.
    #[test]
    fn resumed_plan_steps_keep_the_times_the_provider_measured() {
        let mut state = test_state();
        let thread = json!({
            "turns": [{
                "items": [{
                    "type": "plan",
                    "text": "✔ 1. 확인\n✔ 2. 수정",
                    "steps": [
                        { "step": "1. 확인", "status": "completed", "elapsedMs": 6_000 },
                        { "step": "2. 수정", "status": "completed", "elapsedMs": 4_000 }
                    ]
                }]
            }]
        });

        state.load_history(&thread, None);

        let summary = state.plan_summary.expect("restored plan summary");
        assert_eq!(summary.steps.len(), 2);
        assert_eq!(summary.steps[0].text, "1. 확인");
        assert_eq!(summary.steps[0].elapsed, Some(Duration::from_secs(6)));
        assert_eq!(summary.steps[1].elapsed, Some(Duration::from_secs(4)));
        assert!(
            summary
                .steps
                .iter()
                .all(|step| step.status == PlanStepStatus::Completed)
        );
    }

    #[test]
    fn ansi_stripping_handles_escape_sequences_and_plain_text() {
        for (input, expected) in [
            (
                "\x1b[31mfatal\x1b[0m: no\n\x1b]0;title\x07plain\n\x1b[1;32mok\x1b[m\n",
                "fatal: no\nplain\nok\n",
            ),
            (
                "C:\\Users\\x\\SKILL.md' because it does not exist.\n  ~~~~~\n",
                "C:\\Users\\x\\SKILL.md' because it does not exist.\n  ~~~~~\n",
            ),
        ] {
            assert_eq!(strip_ansi(input), expected);
        }
    }

    #[test]
    fn shift_tab_requests_a_refresh_without_a_plan() {
        let mut state = test_state();
        let action = state.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert!(matches!(action, Action::Tick(true)));
        assert_eq!(state.permission_mode(), PermissionMode::FullAccess);
        assert_eq!(state.permission_profile(), ":danger-full-access");
    }

    /// Shift+Space folds the plan panel wherever the session runs, and the space
    /// it is made of never reaches the composer — not even mid slash command.
    #[test]
    fn shift_space_toggles_the_plan_while_a_slash_command_is_being_typed() {
        let mut state = test_state();
        state.editor.insert_str("/mo");
        state.plan_summary = Some(PlanSummary {
            explanation: None,
            steps: vec![],
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        });

        let action = state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::SHIFT));

        assert!(matches!(action, Action::Tick(true)));
        assert_eq!(state.permission_mode(), PermissionMode::FullAccess);
        assert_eq!(state.editor.text(), "/mo");
        assert!(state.plan_summary.is_some_and(|summary| !summary.expanded));
    }

    /// An unshifted space is still a space, and Shift+Space folds a Claude
    /// session's plan too — Shift+Tab is spoken for there.
    #[test]
    fn shift_space_folds_the_plan_on_every_runtime() {
        let mut state = AppState::new(
            "claude:thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![test_model("claude:sonnet", "Sonnet", true)],
            "claude:sonnet",
            Some("high"),
        );
        state.plan_summary = Some(PlanSummary {
            explanation: None,
            steps: vec![],
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        });
        // A new session reads its starting mode from the settings on disk, so the
        // baseline is pinned here rather than left to whatever the machine saved.
        state.claude_permission_mode = ClaudePermissionMode::Auto;

        state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(state.editor.text(), " ");
        assert!(
            state
                .plan_summary
                .as_ref()
                .is_some_and(|summary| summary.expanded)
        );

        state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::SHIFT));

        assert_eq!(state.editor.text(), " ");
        assert!(
            state
                .plan_summary
                .as_ref()
                .is_some_and(|summary| !summary.expanded)
        );
        assert_eq!(
            state.claude_permission_mode(),
            Some(ClaudePermissionMode::Auto),
            "folding the plan is not a permission change"
        );
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
    fn claude_account_plan_uses_subscription_windows() {
        let account = json!({ "subscriptionType": "team" });
        let usage = json!({
            "subscription_type": "team",
            "rate_limits": {
                "five_hour": { "utilization": 37.4, "resets_at": "2026-08-06T05:00:00Z" },
                "seven_day": { "utilization": 61.0, "resets_at": "2026-08-10T05:00:00Z" }
            }
        });

        let plan = AccountPlan::from_claude(Some(&account), Some(&usage));

        assert_eq!(plan.plan_display(), "Claude Team");
        assert_eq!(plan.five_hour_percent, Some(37));
        assert_eq!(plan.weekly_percent, Some(61));
        assert!(plan.credit_lines()[0].starts_with("5h 37% used · reset "));
        assert!(plan.credit_lines()[1].starts_with("7d 61% used · reset "));
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
    fn clear_command_and_shortcut_share_the_action_and_restore_welcome() {
        let mut state = test_state();
        state.editor.insert_str("hello");
        state.submit_editor();
        assert!(state.view().welcome.is_none());

        assert!(matches!(
            state.run_slash_command("/clear"),
            Action::ClearScreen
        ));
        state.reset_welcome();

        assert!(state.view().welcome.is_some());
        assert!(matches!(
            state.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Action::ClearScreen
        ));
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
        {
            let (method, params, expected) = (
                "model/rerouted",
                json!({ "fromModel": "Sol", "toModel": "Luna" }),
                "Sol → Luna로 전환됨",
            );
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
            let tick = state.render_tick();
            assert!(tick.redraw, "{method} should redraw once it expires");
            assert!(
                !tick.animation_only,
                "{method} expiry changes the full composer frame"
            );
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
    fn claude_login_commands_use_claude_cli_instead_of_chatgpt_overlays() {
        let mut state = test_state();
        state.models = vec![test_model("claude:sonnet", "Claude Sonnet", true)];
        state.selected_model = 0;

        assert!(matches!(state.run_slash_command("/login"), Action::None));
        assert!(state.overlay_view().is_none());
        assert!(
            state
                .committed
                .iter()
                .any(|block| block.body.contains("claude auth login"))
        );

        assert!(matches!(state.run_slash_command("/logout"), Action::None));
        assert!(state.overlay_view().is_none());
        assert!(
            state
                .committed
                .iter()
                .any(|block| block.body.contains("claude auth logout"))
        );
    }

    #[test]
    fn claude_haiku_hides_unsupported_effort_controls() {
        let mut state = test_state();
        let mut haiku = test_model("claude:haiku", "Claude Haiku", true);
        haiku.id = "claude:claude-haiku-4-5-20251001".to_owned();
        haiku.efforts.clear();
        haiku.default_effort.clear();
        state.models = vec![haiku];
        state.selected_model = 0;
        state.selected_effort.clear();

        assert!(state.status_line().effort.is_none());
        assert!(matches!(state.run_slash_command("/model"), Action::None));
        let model_picker = state.overlay_view().expect("model picker");
        assert!(model_picker.slider.is_none());
        assert!(!model_picker.hint.contains("effort"));

        state.pending = None;
        assert!(matches!(state.run_slash_command("/effort"), Action::None));
        assert!(state.overlay_view().is_none());
        assert!(
            state
                .committed
                .last()
                .is_some_and(|block| block.title == "Effort unavailable")
        );
    }

    #[test]
    fn canonical_claude_model_id_selects_its_sdk_alias_row() {
        let mut state = test_state();
        let mut sonnet = test_model("claude:sonnet", "Claude Sonnet", false);
        sonnet.id = "claude:claude-sonnet-5".to_owned();
        state.models = vec![test_model("gpt-5.6-sol", "GPT-5.6 Sol", true), sonnet];

        state.select_model_and_effort("claude:claude-sonnet-5", Some("max"));

        assert_eq!(state.selected_model_name(), "claude:sonnet");
        assert_eq!(state.selected_effort(), "max");
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
    fn global_vibe_settings_replace_one_value_and_preserve_the_others() {
        let config = upsert_vibe_config_value(
            "vibe_mode = \"vibe\"\nshell_display_mode = \"collapse\"\n",
            "shell_display_mode",
            "hide",
        );

        assert!(config.contains("vibe_mode = \"vibe\""));
        assert!(config.contains("shell_display_mode = \"hide\""));
        assert!(!config.contains("shell_display_mode = \"collapse\""));
    }

    #[test]
    fn session_modes_replace_the_same_thread_and_trim_oldest_first() {
        // A thread already on record is updated in place, moved to the newest
        // end, and never duplicated — the way a resume must find exactly one
        // entry for the thread it reopens.
        let sessions = upsert_session_modes(
            vec![(
                "thread-a".to_owned(),
                vec![("vibe_mode".to_owned(), "vibe".to_owned())],
            )],
            "thread-a",
            vec![("vibe_mode".to_owned(), "super_vibe".to_owned())],
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].1[0].1, "super_vibe");

        // Past the history limit the oldest entries fall off the front, so the
        // file cannot grow without bound as sessions accumulate.
        let mut many: Vec<(String, Vec<(String, String)>)> = (0..SESSION_MODE_HISTORY)
            .map(|index| (format!("thread-{index}"), Vec::new()))
            .collect();
        many = upsert_session_modes(many, "thread-new", Vec::new());
        assert_eq!(many.len(), SESSION_MODE_HISTORY);
        assert_eq!(many.last().expect("newest entry").0, "thread-new");
        assert!(!many.iter().any(|(id, _)| id == "thread-0"));
    }

    #[test]
    fn response_display_mode_accepts_only_supported_config_values() {
        assert_eq!(
            ResponseDisplayMode::from_config_value("all"),
            Some(ResponseDisplayMode::All)
        );
        assert_eq!(
            ResponseDisplayMode::from_config_value("Completed"),
            Some(ResponseDisplayMode::Completed)
        );
        assert_eq!(ResponseDisplayMode::from_config_value("compact"), None);
    }

    #[test]
    fn status_command_reports_each_provider_permission_mode() {
        let mut codex = test_state();
        let mut claude = AppState::new(
            "claude:thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![test_model("claude:sonnet", "Sonnet", true)],
            "claude:sonnet",
            Some("high"),
        );
        claude.claude_permission_mode = ClaudePermissionMode::DontAsk;

        for (state, expected) in [
            (&mut codex, "permissions: Full Access (:danger-full-access)"),
            (&mut claude, "permissions: don't ask (dontAsk)"),
        ] {
            state.run_slash_command("/status");
            assert!(
                state
                    .committed
                    .last()
                    .expect("status block")
                    .body
                    .contains(expected),
                "missing {expected}"
            );
        }
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
    fn skills_picker_separates_providers_and_toggles_the_selected_skill() {
        let response = json!({
            "data": [{
                "skills": [{
                    "name": "browser",
                    "path": "C:/claude/plugins/browser/skills/browser/SKILL.md",
                    "description": "Browser automation",
                    "enabled": true,
                    "scope": "user",
                    "pluginId": "browser@official"
                }]
            }]
        });
        let mut state = test_state();
        state.open_skills_picker(SkillProvider::Claude, &response, None);

        let overlay = state.overlay_view().expect("skills picker");
        assert_eq!(overlay.title, "Skills");
        assert_eq!(overlay.lines.len(), 10);
        assert!(overlay.lines[0].text.starts_with("[x] browser"));
        assert!(overlay.lines[0].text.contains("Browser automation"));
        assert!(
            overlay.lines[0]
                .text
                .contains("browser@official plugin 전체 전환")
        );
        assert!(overlay.lines.iter().all(|line| !line.text.contains('\n')));

        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Char(' '))),
            Action::SetSkillEnabled {
                provider: SkillProvider::Claude,
                ref path,
                ref scope,
                enabled: false,
                ..
            } if path.ends_with("browser/SKILL.md") && scope == "user"
        ));
        let overlay = state.overlay_view().expect("optimistic skills picker");
        assert!(overlay.lines[0].text.starts_with("[ ] browser"));
        assert!(overlay.hint.contains("저장 중"));

        state.open_skills_picker(SkillProvider::Claude, &response, None);
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Enter)),
            Action::SetSkillEnabled {
                provider: SkillProvider::Claude,
                enabled: false,
                ..
            }
        ));

        state.open_skills_picker(SkillProvider::Claude, &response, None);
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Right)),
            Action::OpenSkills {
                provider: SkillProvider::Codex,
                notice: None,
            }
        ));
    }

    #[test]
    fn clicking_a_skill_row_toggles_it_even_when_the_title_has_a_notice() {
        let response = json!({
            "data": [{
                "skills": [{
                    "name": "review",
                    "path": "C:/skills/review/SKILL.md",
                    "enabled": false,
                    "scope": "repo"
                }]
            }]
        });
        let mut state = test_state();
        state.open_skills_picker(
            SkillProvider::Codex,
            &response,
            Some("설정이 저장되었습니다.".to_owned()),
        );

        assert!(matches!(
            state.click_overlay_row(0),
            Action::SetSkillEnabled {
                provider: SkillProvider::Codex,
                ref path,
                enabled: true,
                ..
            } if path == "C:/skills/review/SKILL.md"
        ));
    }

    #[test]
    fn skills_picker_keeps_ten_rows_and_scrolls_the_selection() {
        let skills = (1..=12)
            .map(|index| {
                json!({
                    "name": format!("skill-{index:02}"),
                    "path": format!("C:/skills/skill-{index:02}/SKILL.md"),
                    "description": format!("Description {index}"),
                    "enabled": true
                })
            })
            .collect::<Vec<_>>();
        let response = json!({ "data": [{ "skills": skills }] });
        let mut state = test_state();
        state.open_skills_picker(SkillProvider::Codex, &response, None);

        for _ in 0..10 {
            state.handle_key(KeyEvent::from(KeyCode::Down));
        }

        let overlay = state.overlay_view().expect("skills picker");
        assert_eq!(overlay.lines.len(), 10);
        assert!(overlay.lines[0].text.starts_with("[x] skill-03"));
        assert!(overlay.lines[8].text.starts_with("[x] skill-11"));
        assert!(overlay.lines[8].selected);
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
    fn fast_mode_updates_the_composer_badge_and_reports_the_switch() {
        let mut state = test_state();

        state.set_fast_mode(true);

        assert!(state.fast_mode);
        assert!(state.composer_mode().fast_mode);
        assert_eq!(
            state.composer_mode().response_display_mode,
            state.response_display_mode().label()
        );
        assert!(state.transient_status.is_none());
        let on = state
            .committed
            .iter()
            .find(|block| block.title.starts_with("✓ Fast mode"))
            .expect("fast mode notice");
        assert_eq!(on.title, "✓ Fast mode On");

        state.set_fast_mode(false);

        assert!(!state.composer_mode().fast_mode);
        assert_eq!(
            state
                .committed
                .iter()
                .rfind(|block| block.title.starts_with("✓ Fast mode"))
                .map(|block| block.title.as_str()),
            Some("✓ Fast mode Off")
        );
    }

    #[test]
    fn composer_badge_carries_fixed_access_and_response_display_mode() {
        let mut state = test_state();
        state.set_fast_mode(false);
        state.set_response_display_mode(ResponseDisplayMode::Completed);

        let badge = state.composer_mode();

        assert_eq!(badge.label, "Full Access");
        assert_eq!(badge.response_length, "Short");
        assert_eq!(badge.response_display_mode, "Completed");

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
    fn new_version_update_notice_is_english() {
        let mut state = test_state();
        state.show_welcome = false;

        state.push_update_available("1.3.11");
        let blocks = state.drain_committed();

        assert_eq!(blocks[0].title, "Update Available");
        assert_eq!(
            blocks[0].body,
            "New version 1.3.11 is available. Run: dvz update"
        );
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
    fn submitted_prompt_keeps_image_label_with_text() {
        let mut state = test_state();
        state.editor.insert_str("before");
        state.attach_local_image(r"C:\Temp\clipboard-image.bmp".to_owned());
        state.editor.insert_str("after");

        let action = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(action, Action::Submit(text) if text == "beforeafter"));
        assert_eq!(
            state.committed.last().map(|block| block.body.as_str()),
            Some("before [Image #1] after")
        );
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
        assert_eq!(overlay.lines.len(), ThemeKind::ALL.len());
        assert!(overlay.lines[0].text.contains("Minimal"));
        assert!(overlay.lines[5].text.contains("Midnight Blue"));
        state.pending = None;

        assert!(matches!(
            state.run_slash_command("/theme soft"),
            Action::SetTheme(ThemeKind::Soft)
        ));
        let card = state.committed.last().expect("theme card");
        assert_eq!(card.title, "✓ Theme changed");
        assert_eq!(card.body, "↳ Soft");

        assert!(matches!(
            state.run_slash_command("/theme softpink"),
            Action::SetTheme(ThemeKind::SoftPink)
        ));
    }

    #[test]
    fn model_catalog_exposes_the_fast_service_tier() {
        let model = ModelInfo::from_value(&json!({
            "id": "gpt-5.6-sol",
            "model": "gpt-5.6-sol",
            "displayName": "GPT-5.6-Sol",
            "supportedReasoningEfforts": [{"reasoningEffort": "high"}],
            "defaultReasoningEffort": "high",
            "serviceTiers": [{
                "id": "priority",
                "name": "Fast",
                "description": "1.5x speed"
            }]
        }))
        .expect("model");

        assert_eq!(model.display_name, "GPT-5.6 Sol");
        assert_eq!(model.fast_service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn model_display_names_replace_all_gpt_variant_hyphens() {
        assert_eq!(
            normalized_model_display_name("GPT-5.3-Codex-Spark"),
            "GPT-5.3 Codex Spark"
        );
        assert_eq!(
            normalized_model_display_name("Claude Opus 5"),
            "Claude Opus 5"
        );
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
            supports_auto_mode: false,
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
            test_model("gpt-5.6-sol", "GPT-5.6 Sol", true),
            test_model("gpt-5.6-terra", "GPT-5.6 Terra", false),
            test_model("gpt-5.6-luna", "GPT-5.6 Luna", false),
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
        assert_eq!(state.selected_model_display_name(), "GPT-5.6 Terra");

        state.run_slash_command("/model");
        let overlay = state.overlay_view().expect("model picker");
        assert!(overlay.lines[0].text.starts_with("1. "));
        assert!(overlay.lines[1].text.starts_with("2. "));
        // Picking a model now asks how long it should last before applying.
        state.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(state.selected_model_display_name(), "GPT-5.6 Terra");
        assert_eq!(
            state.overlay_view().expect("scope picker").title,
            "Apply to"
        );
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state.selected_model_display_name(), "GPT-5.6 Sol");
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
    fn choosing_the_default_side_panel_scope_applies_and_persists_the_size() {
        let mut state = test_state();
        state.thread_id.clear();
        state.run_slash_command("/side-panel");

        state.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        let overlay = state.overlay_view().expect("side-panel scope picker");
        assert_eq!(overlay.title, "Apply to");
        assert_eq!(overlay.lines[0].text, "Large");

        let action = state.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));

        assert!(matches!(
            action,
            Action::PersistSidePanelDefault(SidePanelStage::Large)
        ));
        assert_eq!(state.side_panel_stage(), SidePanelStage::Large);
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
    fn side_exit_keys_never_interrupt_or_quit_either_turn() {
        let mut state = busy_state_with_live_turn();
        state.enter_side_thread(
            "fork-thread".to_owned(),
            "cwd".to_owned(),
            "gpt-5.6-sol",
            Some("high"),
        );
        state.set_turn_started("side-turn".to_owned());
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Esc)),
            Action::ReturnFromSide
        ));
        assert!(matches!(state.handle_key(ctrl_c), Action::ReturnFromSide));
        assert!(matches!(state.handle_key(ctrl_c), Action::ReturnFromSide));
        assert_eq!(state.turn_id.as_deref(), Some("side-turn"));
        assert!(!state.quit_armed());
        assert_eq!(
            state.take_side_parent_turn().map(|turn| turn.0),
            Some("live-turn".to_owned())
        );
    }

    #[test]
    fn side_escape_closes_before_a_pending_approval_can_consume_it() {
        let mut state = busy_state_with_live_turn();
        state.enter_side_thread(
            "fork-thread".to_owned(),
            "cwd".to_owned(),
            "gpt-5.6-sol",
            Some("high"),
        );
        state.begin_server_request(
            json!(41),
            "item/commandExecution/requestApproval",
            &json!({ "command": "cargo test" }),
        );

        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Esc)),
            Action::ReturnFromSide
        ));
        assert!(state.has_pending_interaction());
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

        assert_eq!(state.activity().as_deref(), Some("✧ Completed (1m 5s)"));
    }

    /// `/compact` has no assistant output of its own, so the activity row is the
    /// only place the wait is visible: it spins until the boundary arrives.
    #[test]
    fn codex_compaction_uses_elapsed_time_until_the_boundary() {
        let mut state = test_state();

        state.begin_compaction();
        state.compacting_started_at = Some(Instant::now() - Duration::from_secs(4));

        assert_eq!(state.activity().as_deref(), Some("Compacting.. (4s)"));
        assert!(state.render_tick().redraw, "the spinner keeps animating");
        assert!(state.host_turn_busy(), "the host tab spins too");

        state.handle_notification("thread/compacted", &json!({}));

        assert!(!state.compacting());
        assert_eq!(state.activity(), None);
    }

    /// A compaction that runs as a turn must not fall back to the `Working` label
    /// when the boundary never arrives — the turn ending still clears it.
    #[test]
    fn a_completed_turn_clears_a_compaction_that_never_reported_its_boundary() {
        let mut state = test_state();
        state.begin_compaction();
        state.handle_notification("turn/started", &json!({ "turn": { "id": "turn-1" } }));

        assert!(
            state
                .activity()
                .is_some_and(|activity| activity.starts_with("Compacting.."))
        );

        state.handle_notification("turn/completed", &json!({}));

        assert!(!state.compacting());
    }

    #[test]
    fn codex_final_answer_folds_only_earlier_commentary() {
        let mut state = test_state();
        while state.vibe_mode() != VibeMode::SuperVibe {
            state.cycle_vibe_mode();
        }
        // 접힘은 Completed 표시 모드에서만 일어난다. 생성자는 이 값을 디스크 설정에서
        // 읽으므로, 머신 설정에 흔들리지 않게 테스트에서 명시한다.
        state.set_response_display_mode(ResponseDisplayMode::Completed);
        state.set_turn_started("turn-1".to_owned());
        for (id, phase, text) in [
            ("progress-1", "commentary", "원격 변경을 확인했습니다."),
            ("progress-2", "commentary", "전체 테스트가 통과했습니다."),
            ("final", "final_answer", "배포를 완료했습니다."),
        ] {
            state.handle_notification(
                "item/completed",
                &json!({
                    "item": { "id": id, "type": "agentMessage", "phase": phase, "text": text }
                }),
            );
            state.drain_committed();
        }

        state.handle_notification(
            "turn/completed",
            &json!({ "turn": { "status": "completed" } }),
        );
        let blocks = state.drain_committed();
        let group = blocks
            .iter()
            .find(|block| matches!(block.kind, BlockKind::ProgressGroup))
            .expect("progress group");

        assert_eq!(group.children().len(), 2);
        assert_eq!(group.title, "+2 Response");
        assert!(state.response_collapse_view().is_some());
    }

    #[test]
    fn completed_mode_folds_progress_before_final_answer_streaming_starts() {
        let mut state = test_state();
        while state.vibe_mode() != VibeMode::SuperVibe {
            state.cycle_vibe_mode();
        }
        state.set_response_display_mode(ResponseDisplayMode::Completed);
        assert!(matches!(
            state.submit_text("첫 요청".to_owned(), "첫 요청".to_owned()),
            Action::Submit(_)
        ));
        state.drain_committed();
        state.set_turn_started("turn-1".to_owned());
        state.complete_item(&json!({
            "id": "compact-1",
            "type": "contextCompaction"
        }));
        state.drain_committed();
        state.handle_notification(
            "item/completed",
            &json!({
                "item": {
                    "id": "progress-1",
                    "type": "agentMessage",
                    "phase": "commentary",
                    "text": "진행 메시지"
                }
            }),
        );
        state.drain_committed();

        state.handle_notification(
            "item/started",
            &json!({
                "item": {
                    "id": "final",
                    "type": "agentMessage",
                    "phase": "final_answer",
                    "text": ""
                }
            }),
        );
        let blocks = state.drain_committed();
        let group = blocks
            .iter()
            .find(|block| matches!(block.kind, BlockKind::ProgressGroup))
            .expect("progress group before final stream");

        assert_eq!(group.title, "+2 Response");
        assert_eq!(group.children()[0].title, "Context compacted");
        assert_eq!(group.children()[1].body, "진행 메시지");
        assert!(state.response_collapse_view().is_none());
        assert!(state.response_grouped);

        state.handle_notification(
            "item/completed",
            &json!({
                "item": {
                    "id": "final",
                    "type": "agentMessage",
                    "phase": "final_answer",
                    "text": "최종 답변"
                }
            }),
        );
        state.drain_committed();
        state.handle_notification(
            "turn/completed",
            &json!({ "turn": { "status": "completed" } }),
        );
        assert!(
            state
                .drain_committed()
                .iter()
                .all(|block| !matches!(block.kind, BlockKind::ProgressGroup))
        );
    }

    #[test]
    fn completed_mode_folds_before_a_final_stream_mislabeled_as_commentary() {
        let mut state = test_state();
        while state.vibe_mode() != VibeMode::SuperVibe {
            state.cycle_vibe_mode();
        }
        state.set_response_display_mode(ResponseDisplayMode::Completed);
        assert!(matches!(
            state.submit_text("짧은 요청".to_owned(), "짧은 요청".to_owned()),
            Action::Submit(_)
        ));
        state.drain_committed();
        state.set_turn_started("turn-1".to_owned());

        state.handle_notification(
            "item/completed",
            &json!({
                "item": {
                    "id": "progress",
                    "type": "agentMessage",
                    "phase": "commentary",
                    "text": "진행 메시지"
                }
            }),
        );
        state.drain_committed();

        state.handle_notification(
            "item/started",
            &json!({
                "item": {
                    "id": "final",
                    "type": "agentMessage",
                    "phase": "commentary",
                    "text": ""
                }
            }),
        );
        let group = state
            .drain_committed()
            .into_iter()
            .find(|block| matches!(block.kind, BlockKind::ProgressGroup))
            .expect("progress is folded before the mislabeled final stream");
        assert_eq!(group.children().len(), 1);

        state.handle_notification(
            "item/completed",
            &json!({
                "item": {
                    "id": "final",
                    "type": "agentMessage",
                    "phase": "final_answer",
                    "text": "최종 답변"
                }
            }),
        );
        state.drain_committed();
        state.handle_notification(
            "turn/completed",
            &json!({ "turn": { "status": "completed" } }),
        );

        assert!(state.drain_committed().is_empty());
        assert!(state.response_collapse_view().is_none());
    }

    #[test]
    fn completed_mode_refolds_unphased_progress_before_each_new_response() {
        let mut state = test_state();
        while state.vibe_mode() != VibeMode::SuperVibe {
            state.cycle_vibe_mode();
        }
        state.set_response_display_mode(ResponseDisplayMode::Completed);
        assert!(matches!(
            state.submit_text("짧은 요청".to_owned(), "짧은 요청".to_owned()),
            Action::Submit(_)
        ));
        state.drain_committed();
        state.set_turn_started("turn-1".to_owned());

        state.handle_notification(
            "item/completed",
            &json!({
                "item": {
                    "id": "claude-progress-1",
                    "type": "agentMessage",
                    "provider": "Claude",
                    "text": "첫 진행 메시지"
                }
            }),
        );
        state.drain_committed();

        state.handle_notification(
            "item/started",
            &json!({
                "item": {
                    "id": "claude-progress-2",
                    "type": "agentMessage",
                    "provider": "Claude",
                    "text": ""
                }
            }),
        );
        let first_group = state
            .drain_committed()
            .into_iter()
            .find(|block| matches!(block.kind, BlockKind::ProgressGroup))
            .expect("first Claude progress is folded before the next stream");
        assert_eq!(first_group.children().len(), 1);

        state.handle_notification(
            "item/completed",
            &json!({
                "item": {
                    "id": "claude-progress-2",
                    "type": "agentMessage",
                    "provider": "Claude",
                    "text": "두 번째 진행 메시지"
                }
            }),
        );
        state.drain_committed();
        state.handle_notification(
            "item/started",
            &json!({
                "item": {
                    "id": "claude-final",
                    "type": "agentMessage",
                    "provider": "Claude",
                    "text": ""
                }
            }),
        );
        let updated_group = state
            .drain_committed()
            .into_iter()
            .find(|block| matches!(block.kind, BlockKind::ProgressGroup))
            .expect("all earlier Claude progress is folded before the final stream");

        assert_eq!(updated_group.id(), first_group.id());
        assert_eq!(updated_group.children().len(), 2);
        assert_eq!(updated_group.children()[1].body, "두 번째 진행 메시지");
        assert!(state.response_grouped);
        assert!(state.response_collapse_view().is_none());

        state.handle_notification(
            "item/completed",
            &json!({
                "item": {
                    "id": "claude-final",
                    "type": "agentMessage",
                    "provider": "Claude",
                    "text": "최종 답변"
                }
            }),
        );
        let completed = state.drain_committed();
        assert!(completed.iter().any(|block| block.body == "최종 답변"));
        state.handle_notification(
            "turn/completed",
            &json!({ "turn": { "status": "completed" } }),
        );
        assert!(state.drain_committed().is_empty());
        assert!(state.response_collapse_view().is_none());
    }

    #[test]
    fn completed_mode_folds_progress_before_a_delta_only_response() {
        let mut state = test_state();
        while state.vibe_mode() != VibeMode::SuperVibe {
            state.cycle_vibe_mode();
        }
        state.set_response_display_mode(ResponseDisplayMode::Completed);
        state.set_turn_started("turn-1".to_owned());
        state.handle_notification(
            "item/completed",
            &json!({
                "item": {
                    "id": "progress",
                    "type": "agentMessage",
                    "provider": "OpenCode",
                    "text": "진행 메시지"
                }
            }),
        );
        state.drain_committed();

        state.handle_notification(
            "item/agentMessage/delta",
            &json!({
                "itemId": "delta-only-answer",
                "provider": "OpenCode",
                "delta": "최종 답변"
            }),
        );
        let group = state
            .drain_committed()
            .into_iter()
            .find(|block| matches!(block.kind, BlockKind::ProgressGroup))
            .expect("progress is folded before the first answer delta");

        assert_eq!(group.children().len(), 1);
        assert_eq!(group.children()[0].body, "진행 메시지");
        assert!(state.active.contains_key("delta-only-answer"));
    }

    #[test]
    fn all_response_mode_keeps_progress_visible_when_final_streaming_starts() {
        let mut state = test_state();
        while state.vibe_mode() != VibeMode::SuperVibe {
            state.cycle_vibe_mode();
        }
        state.set_response_display_mode(ResponseDisplayMode::All);
        state.set_turn_started("turn-1".to_owned());
        state.handle_notification(
            "item/completed",
            &json!({
                "item": {
                    "id": "progress-1",
                    "type": "agentMessage",
                    "phase": "commentary",
                    "text": "진행 메시지"
                }
            }),
        );
        state.drain_committed();

        state.handle_notification(
            "item/started",
            &json!({
                "item": {
                    "id": "final",
                    "type": "agentMessage",
                    "phase": "final_answer",
                    "text": ""
                }
            }),
        );

        assert!(state.drain_committed().is_empty());
        assert!(!state.response_grouped);
    }

    #[test]
    fn steer_splits_progress_history_at_the_new_prompt() {
        let mut state = test_state();
        while state.vibe_mode() != VibeMode::SuperVibe {
            state.cycle_vibe_mode();
        }
        assert!(matches!(
            state.submit_text("첫 요청".to_owned(), "첫 요청".to_owned()),
            Action::Submit(_)
        ));
        state.drain_committed();
        state.set_turn_started("turn-1".to_owned());

        state.handle_notification(
            "item/completed",
            &json!({
                "item": { "id": "progress-1", "type": "agentMessage", "phase": "commentary", "text": "첫 요청 진행 기록" }
            }),
        );
        state.drain_committed();

        assert!(matches!(
            state.submit_text("추가 요청".to_owned(), "추가 요청".to_owned()),
            Action::Steer(_)
        ));
        state.drain_committed();
        for (id, phase, text) in [
            ("progress-2", "commentary", "추가 요청 확인"),
            ("progress-3", "commentary", "추가 요청 수정"),
            ("final", "final_answer", "수정을 마쳤습니다."),
        ] {
            state.handle_notification(
                "item/completed",
                &json!({
                    "item": { "id": id, "type": "agentMessage", "phase": phase, "text": text }
                }),
            );
            state.drain_committed();
        }

        state.handle_notification(
            "turn/completed",
            &json!({ "turn": { "status": "completed" } }),
        );
        let groups = state
            .drain_committed()
            .into_iter()
            .filter(|block| matches!(block.kind, BlockKind::ProgressGroup))
            .collect::<Vec<_>>();

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title, "+1 Response");
        assert_eq!(groups[0].children()[0].body, "첫 요청 진행 기록");
        assert_eq!(groups[1].title, "+2 Response");
        assert_eq!(groups[1].children()[0].body, "추가 요청 확인");
        assert_eq!(groups[1].children()[1].body, "추가 요청 수정");
    }

    #[test]
    fn context_compaction_positions_fold_into_live_history() {
        type Case = (
            &'static str,
            &'static [&'static str],
            &'static [&'static str],
            bool,
            bool,
            usize,
            &'static str,
            bool,
        );
        let cases: &[Case] = &[
            (
                "between updates",
                &["첫 진행 메시지"],
                &["두 번째 진행 메시지"],
                true,
                false,
                1,
                "+3 Response",
                false,
            ),
            (
                "before the first update",
                &[],
                &["첫 진행 메시지", "두 번째 진행 메시지"],
                true,
                true,
                0,
                "+3 Response",
                true,
            ),
            (
                "without an update",
                &[],
                &[],
                false,
                false,
                0,
                "+1 Response",
                true,
            ),
        ];

        for &(
            name,
            before,
            after,
            report_boundary,
            submit_prompt,
            compact_index,
            expected_title,
            expect_collapse,
        ) in cases
        {
            let mut state = test_state();
            while state.vibe_mode() != VibeMode::SuperVibe {
                state.cycle_vibe_mode();
            }
            // 생성자가 디스크 설정에서 읽는 값에 흔들리지 않도록 Completed를 명시한다.
            state.set_response_display_mode(ResponseDisplayMode::Completed);
            if submit_prompt {
                assert!(matches!(
                    state.submit_text("첫 요청".to_owned(), "첫 요청".to_owned()),
                    Action::Submit(_)
                ));
                state.drain_committed();
            }
            state.set_turn_started("turn-1".to_owned());

            for (index, text) in before.iter().enumerate() {
                state.handle_notification(
                    "item/completed",
                    &json!({
                        "item": {
                            "id": format!("before-{index}"),
                            "type": "agentMessage",
                            "phase": "commentary",
                            "text": text
                        }
                    }),
                );
                state.drain_committed();
            }
            state.complete_item(&json!({
                "id": "compact-1",
                "type": "contextCompaction"
            }));
            if report_boundary {
                state.handle_notification("thread/compacted", &json!({}));
            }
            state.drain_committed();
            for (index, text) in after.iter().enumerate() {
                state.handle_notification(
                    "item/completed",
                    &json!({
                        "item": {
                            "id": format!("after-{index}"),
                            "type": "agentMessage",
                            "phase": "commentary",
                            "text": text
                        }
                    }),
                );
                state.drain_committed();
            }
            state.handle_notification(
                "item/completed",
                &json!({
                    "item": {
                        "id": "final",
                        "type": "agentMessage",
                        "phase": "final_answer",
                        "text": "최종 답변"
                    }
                }),
            );
            state.drain_committed();
            state.handle_notification(
                "turn/completed",
                &json!({ "turn": { "status": "completed" } }),
            );
            let blocks = state.drain_committed();
            let group = blocks
                .iter()
                .find(|block| matches!(block.kind, BlockKind::ProgressGroup))
                .unwrap_or_else(|| panic!("missing progress group: {name}"));

            assert_eq!(group.title, expected_title, "{name}");
            assert_eq!(
                group.children()[compact_index].title,
                "Context compacted",
                "{name}"
            );
            if expect_collapse {
                assert!(state.response_collapse_view().is_some(), "{name}");
            }
        }
    }

    #[test]
    fn context_compaction_positions_fold_into_resumed_history() {
        for (name, blocks, expected_title, compact_index) in [
            (
                "between updates",
                vec![
                    Block::new(BlockKind::Assistant, "Codex", "첫 진행 메시지")
                        .with_assistant_phase(AssistantPhase::Commentary),
                    Block::new(BlockKind::System, "Context compacted", ""),
                    Block::new(BlockKind::Assistant, "Codex", "두 번째 진행 메시지")
                        .with_assistant_phase(AssistantPhase::Commentary),
                    Block::new(BlockKind::Assistant, "Codex", "최종 답변")
                        .with_assistant_phase(AssistantPhase::FinalAnswer),
                ],
                "+3 Response",
                1,
            ),
            (
                "before the first update",
                vec![
                    Block::new(BlockKind::System, "Context compacted", ""),
                    Block::new(BlockKind::Assistant, "Codex", "첫 진행 메시지")
                        .with_assistant_phase(AssistantPhase::Commentary),
                    Block::new(BlockKind::Assistant, "Codex", "최종 답변")
                        .with_assistant_phase(AssistantPhase::FinalAnswer),
                ],
                "+2 Response",
                0,
            ),
        ] {
            let grouped = group_turn_response(blocks);
            let group = grouped
                .iter()
                .find(|block| matches!(block.kind, BlockKind::ProgressGroup))
                .unwrap_or_else(|| panic!("missing progress group: {name}"));

            assert_eq!(group.title, expected_title, "{name}");
            assert_eq!(
                group.children()[compact_index].title,
                "Context compacted",
                "{name}"
            );
        }
    }

    #[test]
    fn resumed_steer_history_stays_with_each_prompt() {
        let blocks = vec![
            Block::new(BlockKind::User, "Codex", "첫 요청"),
            Block::new(BlockKind::Assistant, "Codex", "첫 요청 진행 기록")
                .with_assistant_phase(AssistantPhase::Commentary),
            Block::new(BlockKind::User, "Codex", "추가 요청"),
            Block::new(BlockKind::Assistant, "Codex", "추가 요청 확인")
                .with_assistant_phase(AssistantPhase::Commentary),
            Block::new(BlockKind::System, "Context compacted", ""),
            Block::new(BlockKind::Assistant, "Codex", "최종 답변")
                .with_assistant_phase(AssistantPhase::FinalAnswer),
        ];

        let grouped = group_turn_response(blocks);
        let groups = grouped
            .iter()
            .filter(|block| matches!(block.kind, BlockKind::ProgressGroup))
            .collect::<Vec<_>>();

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].children()[0].body, "첫 요청 진행 기록");
        assert_eq!(groups[1].children()[0].body, "추가 요청 확인");
        assert_eq!(groups[1].children()[1].title, "Context compacted");
    }

    #[test]
    fn ordinary_vibe_modes_do_not_start_response_collapse_animation() {
        for mode in [VibeMode::Vibe, VibeMode::Normal] {
            let mut state = test_state();
            while state.vibe_mode() != mode {
                state.cycle_vibe_mode();
            }
            state.set_turn_started("turn-1".to_owned());
            for (id, text) in [("progress", "확인 중입니다."), ("final", "완료했습니다.")]
            {
                state.handle_notification(
                    "item/completed",
                    &json!({ "item": { "id": id, "type": "agentMessage", "text": text } }),
                );
                state.drain_committed();
            }

            state.handle_notification(
                "turn/completed",
                &json!({ "turn": { "status": "completed" } }),
            );

            assert!(state.response_collapse_view().is_none());
            assert!(!state.view().fold_progress_groups);
        }
    }

    #[test]
    fn claude_uses_the_last_successful_message_but_never_folds_an_interruption() {
        let mut state = test_state();
        state.set_turn_started("turn-1".to_owned());
        for (id, text) in [("progress", "확인 중입니다."), ("last", "완료했습니다.")] {
            state.handle_notification(
                "item/completed",
                &json!({ "item": { "id": id, "type": "agentMessage", "provider": "Claude", "text": text } }),
            );
            state.drain_committed();
        }
        state.turn_interrupted = true;

        state.handle_notification(
            "turn/completed",
            &json!({ "turn": { "status": "completed" } }),
        );

        assert!(
            state
                .drain_committed()
                .iter()
                .all(|block| !matches!(block.kind, BlockKind::ProgressGroup))
        );
    }

    const TEST_FRAME: Duration = STREAM_FRAME_FOR_TESTS;
    /// The loop's tick sits well under one character's worth of time, so a test
    /// that wants to see text has to run several of them.
    const STREAM_FRAME_FOR_TESTS: Duration = Duration::from_millis(4);

    fn drain_frames(state: &mut AppState, frames: usize) {
        for _ in 0..frames {
            state.drain_stream_text(TEST_FRAME);
        }
    }

    #[test]
    fn streamed_text_is_revealed_on_frames_instead_of_on_arrival() {
        let mut state = test_state();
        let text = "한 문장이 통째로 도착해도 화면에는 나눠서 드러납니다.";
        state.handle_notification(
            "item/agentMessage/delta",
            &json!({ "itemId": "item-1", "delta": text }),
        );

        assert_eq!(state.active["item-1"].block.body, "");

        drain_frames(&mut state, 25);
        let first = state.active["item-1"].block.body.clone();
        assert!(!first.is_empty());
        assert!(first.chars().count() < text.chars().count());

        drain_frames(&mut state, 2000);
        assert_eq!(state.active["item-1"].block.body, text);
    }

    /// A pass the loop took twice as long to reach owes twice as much text.
    /// Sizing by frame count instead is what let a slow repaint stall the pace.
    #[test]
    fn a_longer_gap_reveals_proportionally_more_text() {
        let text = "이 문장은 한 번에 다 드러나지 않을 만큼 충분히 길게 이어집니다.".repeat(40);
        let revealed = |elapsed| {
            let mut pace = TextPace::default();
            pace.push(&text);
            pace.take(elapsed).map(|chunk| chunk.chars().count())
        };

        let one = revealed(TEST_FRAME * 5).expect("a short gap reveals text");
        let two = revealed(TEST_FRAME * 10).expect("a longer gap reveals text");
        assert!(two > one, "{two} should exceed {one}");
    }

    /// Deltas arrive in bursts with short gaps between them. Clearing the pace at
    /// every gap would restart each burst from the slowest rate.
    #[test]
    fn a_gap_between_bursts_keeps_the_pace_it_reached() {
        let mut pace = TextPace::default();
        pace.push(&"흐름을 유지하는지 확인하는 긴 문장입니다.".repeat(6));
        for _ in 0..10 {
            pace.take(TEST_FRAME);
        }
        let reached = pace.rate;
        assert!(reached > STREAM_MIN_RATE);

        // Drained dry, then the next burst lands.
        while !pace.pending.is_empty() {
            pace.take(TEST_FRAME);
        }
        assert!(pace.take(TEST_FRAME).is_none());
        assert!(pace.rate >= reached * 0.9, "{} vs {reached}", pace.rate);
    }

    /// The settling tail follows the text: it grows while characters arrive and
    /// retreats once they stop, so a finished answer never sits half-lit.
    #[test]
    fn the_settling_tail_grows_while_text_flows_and_clears_after_it() {
        let mut state = test_state();
        state.handle_notification(
            "item/agentMessage/delta",
            &json!({ "itemId": "item-1", "delta": "글자가 흐르는 동안 꼬리가 자랍니다.".repeat(20) }),
        );

        drain_frames(&mut state, 60);
        assert!(state.stream_fade_tail > 0.0);

        // Nothing left to reveal, so time alone takes the tail back.
        drain_frames(&mut state, 4000);
        assert_eq!(state.stream_fade_tail, 0.0);
    }

    /// The turn ends while the last words are still being revealed, so the notice
    /// waits for them. It must wait, not discard: every character still reaches
    /// the transcript.
    #[test]
    fn a_finished_turn_never_drops_text_that_has_not_been_shown() {
        let mut state = test_state();
        state.handle_notification(
            "item/agentMessage/delta",
            &json!({ "itemId": "item-1", "delta": "아직 드러나지 않은 글자" }),
        );
        state.handle_notification("turn/completed", &json!({}));

        assert!(state.committed.is_empty());

        drain_frames(&mut state, 2000);

        assert!(
            state
                .committed
                .iter()
                .any(|block| block.body == "아직 드러나지 않은 글자")
        );
    }

    #[test]
    fn the_last_revealed_chunk_stays_live_before_completion() {
        let mut state = test_state();
        let text = "마지막 글자가 줄 경계를 넘더라도 완료 전 라이브 화면에 먼저 보입니다.";
        state.handle_notification(
            "item/agentMessage/delta",
            &json!({ "itemId": "item-1", "delta": text }),
        );
        state.handle_notification("turn/completed", &json!({}));

        for _ in 0..2000 {
            let reveal = state.drain_stream_text(TEST_FRAME);
            if state
                .active
                .get("item-1")
                .is_some_and(|item| item.block.body == text)
            {
                assert!(!reveal.released);
                assert!(!state.held_notifications.is_empty());
                assert!(state.drain_committed().is_empty());
                return;
            }
        }

        panic!("the full live answer was never revealed");
    }

    /// A provider can hand over more text than the reveal can clear in any
    /// reasonable time. The wait is bounded, but the flushed text still gets a
    /// live frame before completion moves it into history.
    #[test]
    fn an_expired_finish_paints_its_full_live_frame_before_completing() {
        let mut state = test_state();
        let text = "아주 긴 마무리 문장입니다.".repeat(400);
        state.handle_notification(
            "item/agentMessage/delta",
            &json!({ "itemId": "item-1", "delta": text }),
        );
        state.handle_notification("turn/completed", &json!({}));
        assert!(!state.held_notifications.is_empty());

        state.held_since = Some(Instant::now() - HELD_NOTIFICATION_LIMIT);
        let reveal = state.drain_stream_text(TEST_FRAME);

        assert!(reveal.final_frame_ready);
        assert!(!state.held_notifications.is_empty());
        assert_eq!(state.active["item-1"].block.body, text);
        assert!(state.drain_committed().is_empty());

        for _ in 0..FINAL_STREAM_FRAME_TICKS {
            let reveal = state.drain_stream_text(TEST_FRAME);
            assert!(!reveal.released);
            assert!(!state.held_notifications.is_empty());
        }
        let reveal = state.drain_stream_text(TEST_FRAME);

        assert!(reveal.released);
        assert!(state.held_notifications.is_empty());
        assert!(state.committed.iter().any(|block| block.body == text));
    }

    #[test]
    fn a_joined_emoji_is_never_split_across_frames() {
        let family = "👨‍👩‍👧‍👦";
        assert_eq!(visible_cluster_count(family), 1);
        assert_eq!(visible_cluster_end(family, 0), 0);
        assert_eq!(visible_cluster_end(family, 1), family.len());
        assert_eq!(visible_cluster_count("가나다"), 3);
        assert_eq!(visible_cluster_end("가나다", 2), "가나".len());
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

        assert!(
            state
                .activity()
                .is_some_and(|activity| activity.starts_with("Working.. (10s)"))
        );

        state.select_model_and_effort("gpt-5.6-sol", Some("medium"));
        state.handle_notification("turn/completed", &json!({}));
        assert_eq!(state.activity().as_deref(), Some("✧ Completed (10s)"));
    }

    #[test]
    fn previous_response_waits_only_until_new_assistant_text_appears() {
        let mut state = test_state();
        state.last_assistant_markdown = Some("previous response".to_owned());
        state.editor.set_text("next prompt");

        assert!(matches!(state.submit_editor(), Action::Submit(_)));
        assert!(state.view().waiting_for_response);

        state.handle_notification(
            "item/agentMessage/delta",
            &json!({ "itemId": "answer", "delta": "new response" }),
        );

        assert!(!state.view().waiting_for_response);
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
    fn a_bare_session_id_is_read_as_this_thread_not_a_background_one() {
        let mut state = test_state();
        state.thread_id = "claude:session-one".to_owned();
        state.set_turn_started("turn-1".to_owned());

        state.handle_notification("turn/completed", &json!({ "threadId": "session-one" }));

        assert!(!state.busy, "the bare form names this same thread");
    }

    #[test]
    fn a_quiet_turn_is_probed_once_per_window_and_a_busy_one_never_is() {
        let mut state = test_state();
        state.set_turn_started("turn-1".to_owned());

        assert_eq!(state.take_stall_probe(), None, "a fresh turn is not quiet");

        state.turn_progress_at = Some(Instant::now() - TURN_STALL_SILENCE);
        assert_eq!(state.take_stall_probe().as_deref(), Some("turn-1"));
        assert_eq!(state.take_stall_probe(), None, "one probe per window");

        // Any word from the thread means the turn is alive after all.
        state.handle_notification("item/started", &json!({}));
        state.stall_probe_at = None;
        assert_eq!(state.take_stall_probe(), None);
    }

    #[test]
    fn a_probe_that_finds_the_turn_over_ends_the_wait() {
        let mut state = test_state();
        state.set_turn_started("turn-1".to_owned());
        state.turn_progress_at = Some(Instant::now() - TURN_STALL_SILENCE);
        let turn_id = state.take_stall_probe().expect("probe");

        assert!(state.resolve_stall_probe(&turn_id));
        assert!(!state.busy);
        assert!(
            state
                .activity()
                .is_some_and(|activity| activity.starts_with("✧ Completed"))
        );
        // A stale answer about a turn that is no longer the live one changes nothing.
        state.set_turn_started("turn-2".to_owned());
        assert!(!state.resolve_stall_probe("turn-1"));
        assert!(state.busy);
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

        assert_eq!(state.activity().as_deref(), Some("X Interrupted"));

        state.handle_notification("turn/completed", &json!({}));
        assert_eq!(state.activity().as_deref(), Some("X Interrupted"));
    }

    #[test]
    fn copy_notice_keeps_the_activity_and_uses_the_composer_notice() {
        let mut state = test_state();
        state.busy = true;
        state.turn_started_at = Some(Instant::now() - Duration::from_secs(10));

        state.set_copy_notice();

        assert!(
            state
                .activity()
                .is_some_and(|activity| activity.starts_with("Working.."))
        );
        assert_eq!(
            state.view().composer_notice.as_deref(),
            Some("• Copied to clipboard")
        );
        assert_eq!(
            state.animation_view().composer_notice,
            Some("• Copied to clipboard")
        );
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
    fn plan_notification_commits_the_welcome_card() {
        let mut state = test_state();
        assert!(state.view().welcome.is_some());

        state.handle_notification(
            "turn/plan/updated",
            &json!({
                "plan": [{ "step": "check", "status": "inProgress" }]
            }),
        );

        assert!(state.view().welcome.is_none());
        assert!(matches!(state.committed[0].kind, BlockKind::Welcome));
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
    fn an_unchanged_busy_tick_only_animates_the_activity_rows() {
        let mut state = busy_state_with_live_turn();

        let tick = state.render_tick();

        assert!(tick.redraw);
        assert!(tick.animation_only);
    }

    #[test]
    fn tab_during_a_turn_queues_the_composer_text() {
        let mut state = busy_state_with_live_turn();
        state.editor.set_text("next prompt");

        let action = state.handle_key(KeyEvent::from(KeyCode::Tab));

        assert!(matches!(action, Action::None));
        assert!(state.editor.is_empty());
        assert_eq!(
            state.queued_prompts.front().map(String::as_str),
            Some("next prompt")
        );
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
    fn prompt_after_a_busy_provider_switch_waits_for_the_new_provider() {
        let models = vec![
            test_model("gpt-5.6-sol", "GPT-5.6 Sol", true),
            test_model("claude:sonnet", "Claude Sonnet 5", false),
        ];
        let mut state = AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            models,
            "gpt-5.6-sol",
            Some("high"),
        );
        state.busy = true;
        state.turn_id = Some("codex-turn".to_owned());
        state.active_turn_model = Some("gpt-5.6-sol".to_owned());
        state.run_slash_command("/provider claude");
        state.editor.set_text("Claude로 이어서 처리해");

        let action = state.handle_key(KeyEvent::from(KeyCode::Enter));

        assert!(matches!(action, Action::None));
        assert_eq!(
            state.queued_prompts.front().map(String::as_str),
            Some("Claude로 이어서 처리해")
        );
        assert!(
            state
                .view()
                .composer_placeholder
                .contains("switched provider")
        );

        state.handle_notification("turn/completed", &json!({}));
        let queued = state.take_queued_prompt().unwrap();
        assert!(matches!(
            state.start_queued_prompt(queued),
            Action::Submit(_)
        ));
        assert_eq!(state.selected_model_name(), "claude:sonnet");
    }

    #[test]
    fn handoff_pending_blocks_include_completion_but_not_the_new_target_prompt() {
        let mut state = test_state();
        let completed = Block::new(BlockKind::Assistant, "Codex", "방금 완료한 답변");
        let completed_id = completed.id();
        state.committed.push(completed);
        state
            .committed
            .push(Block::new(BlockKind::User, "Claude", "새 Provider 요청"));

        let blocks = state.pending_provider_handoff_blocks();

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].body, "방금 완료한 답변");
        assert_eq!(state.last_pending_handoff_block_id(), completed_id);
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
    fn a_stale_quit_arm_expires_instead_of_quitting() {
        let mut state = test_state();
        state.set_turn_started("turn-1".to_owned());
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert!(matches!(state.handle_key(ctrl_c), Action::Interrupt));
        // The warning has long faded by the time the next Ctrl+C arrives.
        state.quit_armed_at = Some(Instant::now() - QUIT_ARM_WINDOW - Duration::from_secs(1));

        assert!(matches!(state.handle_key(ctrl_c), Action::Interrupt));
        assert_eq!(
            state.activity().as_deref(),
            Some("• Ctrl+C 한 번 더 누르면 종료합니다.")
        );
    }

    #[test]
    fn quit_warning_visibility_matches_the_arm_window() {
        let mut active = test_state();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert!(matches!(active.handle_key(ctrl_c), Action::None));
        let (_, shown_at, ttl) = active.activity_notice.clone().expect("quit notice");
        assert_eq!(ttl, QUIT_ARM_WINDOW);
        active.activity_notice = Some((
            "• Ctrl+C 한 번 더 누르면 종료합니다.".to_owned(),
            shown_at - NOTICE_TTL - Duration::from_millis(100),
            ttl,
        ));
        assert_eq!(
            active.activity().as_deref(),
            Some("• Ctrl+C 한 번 더 누르면 종료합니다.")
        );
        assert!(active.quit_armed());

        let mut expired = test_state();
        expired.handle_key(ctrl_c);
        let (notice, shown_at, ttl) = expired.activity_notice.clone().expect("quit notice");
        expired.activity_notice = Some((notice, shown_at - ttl - Duration::from_millis(1), ttl));
        assert_eq!(expired.activity(), None);
    }

    #[test]
    fn copying_and_typing_both_disarm_the_quit() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        let mut copied = test_state();
        assert!(matches!(copied.handle_key(ctrl_c), Action::None));
        copied.set_copy_notice();
        assert!(!copied.quit_armed());
        assert!(matches!(copied.handle_key(ctrl_c), Action::None));

        let mut pasted = test_state();
        assert!(matches!(pasted.handle_key(ctrl_c), Action::None));
        pasted.handle_paste("hello");
        assert!(!pasted.quit_armed());
        // The composer now has text, so Ctrl+C clears it rather than quitting.
        assert!(matches!(pasted.handle_key(ctrl_c), Action::None));
        assert!(pasted.editor.is_empty());
        assert!(!pasted.quit_armed());
    }

    #[test]
    fn clearing_the_composer_with_ctrl_c_does_not_leave_the_quit_armed() {
        let mut state = test_state();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        state.handle_paste("draft");

        assert!(matches!(state.handle_key(ctrl_c), Action::None));
        assert!(state.editor.is_empty());
        assert!(!state.quit_armed());
        // The first Ctrl+C on the now-empty composer only arms the quit.
        assert!(matches!(state.handle_key(ctrl_c), Action::None));
        assert!(matches!(state.handle_key(ctrl_c), Action::Quit));
    }

    #[test]
    fn status_metadata_parses_usage_and_fast_mode() {
        let usage = json!({
            "five_hour": { "used_percent": 12.4, "resets_at": 1_786_585_603u64 },
            "weekly": { "used_percent": 70 }
        });

        assert_eq!(
            parse_codex_usage(&usage),
            (Some(12), Some(70), Some(1_786_585_603))
        );
        // Codex accounts without a 5h window report nothing to count down.
        assert_eq!(
            parse_codex_usage(&json!({ "weekly": { "used_percent": 58 } })),
            (None, Some(58), None)
        );
        assert_eq!(
            remaining_label(Some(1_000 + 3 * 3_600 + 33 * 60), 1_000).as_deref(),
            Some("3h 33m")
        );
        assert_eq!(
            remaining_label(Some(1_000 + 420), 1_000).as_deref(),
            Some("7m")
        );
        assert!(remaining_label(Some(900), 1_000).is_none());
        assert!(remaining_label(None, 1_000).is_none());
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

    fn command_approval_state() -> AppState {
        let mut state = test_state();
        let action = state.begin_server_request(
            json!(21),
            "item/commandExecution/requestApproval",
            &json!({ "command": "cargo test", "cwd": "D:\\repo" }),
        );
        assert!(matches!(action, Action::None));
        state
    }

    #[test]
    fn command_approval_ignores_shortcut_and_escape_keys() {
        let mut state = command_approval_state();
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('a'),
            KeyCode::Char('n'),
            KeyCode::Char('ㅛ'),
            KeyCode::Char('ㅜ'),
            KeyCode::Esc,
            KeyCode::Tab,
        ] {
            assert!(matches!(
                state.handle_key(KeyEvent::from(code)),
                Action::None
            ));
            assert!(state.pending.is_some());
        }

        assert!(state.paste_as_prompt_answer("y").is_none());
        assert!(state.pending.is_some());
    }

    #[test]
    fn command_approval_moves_with_arrows_and_confirms_with_enter() {
        let mut state = command_approval_state();
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Down)),
            Action::None
        ));
        let overlay = state.overlay_view().expect("approval selection");
        assert!(overlay.lines[3].selected);
        assert_eq!(overlay.hint, "↑↓ 선택   Enter 확정");
        match state.handle_key(KeyEvent::from(KeyCode::Enter)) {
            Action::RpcResponse { result, .. } => {
                assert_eq!(
                    result.get("decision").and_then(Value::as_str),
                    Some("acceptForSession")
                );
            }
            _ => panic!("selected session approval should be confirmed"),
        }

        let mut state = command_approval_state();
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Down)),
            Action::None
        ));
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Down)),
            Action::None
        ));
        match state.handle_key(KeyEvent::from(KeyCode::Enter)) {
            Action::RpcResponse { result, .. } => {
                assert_eq!(
                    result.get("decision").and_then(Value::as_str),
                    Some("decline")
                );
            }
            _ => panic!("selected decline should be confirmed"),
        }
    }

    #[test]
    fn command_approval_rows_answer_to_clicks() {
        // detail 두 줄(명령, 위치) 뒤에 승인 선택지 세 행이 온다.
        let mut state = command_approval_state();
        let clicked = state.click_overlay_row(0);
        assert!(matches!(clicked, Action::Tick(false)));
        assert!(state.pending.is_some(), "detail rows are not answers");

        match state.click_overlay_row(2) {
            Action::RpcResponse { result, .. } => {
                assert_eq!(
                    result.get("decision").and_then(Value::as_str),
                    Some("accept")
                );
            }
            _ => panic!("the once row should accept"),
        }

        let mut state = command_approval_state();
        match state.click_overlay_row(3) {
            Action::RpcResponse { result, .. } => {
                assert_eq!(
                    result.get("decision").and_then(Value::as_str),
                    Some("acceptForSession")
                );
            }
            _ => panic!("the session row should accept for the session"),
        }

        let mut state = command_approval_state();
        match state.click_overlay_row(4) {
            Action::RpcResponse { result, .. } => {
                assert_eq!(
                    result.get("decision").and_then(Value::as_str),
                    Some("decline")
                );
            }
            _ => panic!("the decline row should decline"),
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

        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('2'),
            KeyCode::Tab,
            KeyCode::Esc,
        ] {
            assert!(matches!(
                state.handle_key(KeyEvent::from(code)),
                Action::None
            ));
            assert!(matches!(
                state.pending,
                Some(PendingInteraction::McpApproval(ref approval)) if approval.selected == 0
            ));
        }

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
    fn mcp_approval_arrows_stop_at_the_first_and_last_option() {
        let mut state = test_state();
        state.begin_server_request(
            json!(12),
            "mcpServer/elicitation/request",
            &json!({
                "serverName": "calendar",
                "message": "Create an event?",
                "mode": "form",
                "requestedSchema": { "type": "object", "properties": {} },
                "_meta": {
                    "codex_approval_kind": "mcp_tool_call",
                    "persist": ["session", "always"]
                }
            }),
        );
        let options = match state.pending {
            Some(PendingInteraction::McpApproval(ref approval)) => approval.options.len(),
            _ => panic!("approval should be pending"),
        };
        assert!(options > 1);

        // Held past the end, the selection has to sit on the last option rather
        // than run off it: an index past the options answers nothing at all.
        for _ in 0..options + 3 {
            state.handle_key(KeyEvent::from(KeyCode::Down));
        }
        assert!(matches!(
            state.pending,
            Some(PendingInteraction::McpApproval(ref approval))
                if approval.selected == options - 1
        ));
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Enter)),
            Action::RpcResponse { .. }
        ));

        // And the same going the other way.
        let mut state = test_state();
        state.begin_server_request(
            json!(13),
            "mcpServer/elicitation/request",
            &json!({
                "serverName": "calendar",
                "message": "Create an event?",
                "mode": "form",
                "requestedSchema": { "type": "object", "properties": {} },
                "_meta": { "codex_approval_kind": "mcp_tool_call" }
            }),
        );
        for _ in 0..3 {
            state.handle_key(KeyEvent::from(KeyCode::Up));
        }
        assert!(matches!(
            state.pending,
            Some(PendingInteraction::McpApproval(ref approval)) if approval.selected == 0
        ));
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
    fn composer_backspace_variants_remove_an_explicit_image_attachment() {
        for key in [
            KeyEvent::from(KeyCode::Backspace),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
        ] {
            let mut state = test_state();
            state.attach_local_image(r"C:\Temp\clipboard-image.bmp".to_owned());
            state.handle_key(key);
            assert_eq!(state.composer_image_count(), 0, "key: {key:?}");
        }
    }

    #[test]
    fn drag_selection_removes_only_the_attachment_it_covers() {
        let mut covered = test_state();
        covered.editor.set_text("before ");
        covered.attach_local_image(r"C:\Temp\clipboard-image.bmp".to_owned());
        covered.editor.insert_str(" after");

        // "before " is seven characters, so the attachment is the eighth.
        assert!(covered.delete_composer_selection(7..8));
        assert_eq!(covered.editor.text(), "before  after");
        assert_eq!(covered.composer_image_count(), 0);

        let mut outside = test_state();
        outside.editor.set_text("keep");
        assert!(!outside.delete_composer_selection(9..9));
        assert_eq!(outside.editor.text(), "keep");
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
    fn resumed_user_prompt_keeps_its_turn_model() {
        let mut state = test_state();
        let thread = json!({
            "turns": [{
                "id": "turn-1",
                "startedAt": 2_i64,
                "completedAt": 3_i64,
                "items": [{
                    "type": "userMessage",
                    "content": [{ "type": "text", "text": "이전 프롬프트" }]
                }]
            }]
        });
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"1970-01-01T00:00:02.000Z","type":"turn_context","payload":{"model":"gpt-5.6-terra"}}"#,
        );

        state.load_history(&thread, Some(&rollout));

        assert_eq!(state.committed[0].title, "gpt-5.6-terra");
    }

    #[test]
    fn resumed_claude_prompt_uses_the_model_stored_by_the_bridge() {
        let mut state = test_state();
        let thread = json!({
            "turns": [{
                "id": "claude-turn-1",
                "model": "claude:claude-haiku-4-5-20251001",
                "items": [{
                    "type": "userMessage",
                    "model": "claude:claude-haiku-4-5-20251001",
                    "content": [{ "type": "text", "text": "hay zzz" }]
                }]
            }]
        });

        state.load_history(&thread, None);

        assert_eq!(state.committed[0].title, "claude:claude-haiku-4-5-20251001");
    }

    #[test]
    fn plan_shimmer_runs_once_after_an_update_then_clears() {
        let mut state = test_state();
        state.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "inProgress" }] }),
        );

        assert!(state.plan_shimmer_phase().is_some());
        state.plan_shimmer_started_at = Some(Instant::now() - PLAN_SHIMMER_DURATION);
        let tick = state.render_tick();
        assert!(tick.redraw);
        assert!(!tick.animation_only);
        assert!(state.plan_shimmer_phase().is_none());
    }

    #[test]
    fn composer_ctrl_delete_deletes_the_next_word() {
        let mut state = test_state();
        state.handle_paste("first second third");
        state.editor.move_home();

        state.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL));

        assert_eq!(state.editor.text(), "second third");
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
    fn disabled_connect_is_hidden_from_slash_command_suggestions() {
        let mut state = test_state();
        state.editor.insert_str("/con");

        assert!(
            state
                .matching_slash_commands()
                .iter()
                .all(|command| command.name != "/connect")
        );
    }

    #[test]
    fn claude_hides_fast_from_help_and_slash_suggestions() {
        let mut state = test_state();
        state.models = vec![test_model("claude:sonnet", "Claude Sonnet", true)];
        state.selected_model = 0;
        state.editor.insert_str("/f");

        assert!(
            state
                .matching_slash_commands()
                .iter()
                .all(|command| command.name != "/fast")
        );

        state.editor.clear();
        assert!(matches!(state.run_slash_command("/help"), Action::None));
        assert!(
            state
                .committed
                .last()
                .is_some_and(|block| !block.body.contains("/fast"))
        );
    }

    #[test]
    fn provider_commands_filter_model_picker_and_direct_selection() {
        let models = vec![
            test_model("gpt-5.6-sol", "GPT-5.6 Sol", true),
            test_model("claude:sonnet", "Claude Sonnet 5", false),
            test_model("claude:claude-opus-4-8", "Claude Opus 4.8", false),
            test_model("gpt-5.6-terra", "GPT-5.6 Terra", false),
            test_model("claude:haiku", "Claude Haiku 4.5", false),
        ];
        let mut state = AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            models,
            "gpt-5.6-sol",
            Some("high"),
        );
        // Both runtimes already picked on this machine, so the commands are plain
        // switches rather than first-time connections.
        state.claude_provider_enabled = true;
        state.codex_provider_enabled = true;

        state.run_slash_command("/model sonnet");
        assert_eq!(state.selected_model_name(), "gpt-5.6-sol");

        state.run_slash_command("/provider claude");
        assert_eq!(state.selected_model_name(), "claude:sonnet");
        assert_eq!(
            state.committed.last().map(|block| block.title.as_str()),
            Some("✓ Provider changed")
        );

        state.run_slash_command("/model");
        let overlay = state.overlay_view().expect("Claude model picker");
        let model_lines = overlay
            .lines
            .iter()
            .filter(|line| !line.text.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(model_lines.len(), 3);
        assert_eq!(
            model_lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            [
                "1. Claude Sonnet 5",
                "2. Claude Opus 4.8",
                "3. Claude Haiku 4.5"
            ]
        );
        state.pending = None;

        state.run_slash_command("/model claude-opus-4-8");
        assert_eq!(state.selected_model_name(), "claude:claude-opus-4-8");

        state.run_slash_command("/model 3");
        assert_eq!(state.selected_model_name(), "claude:haiku");

        assert!(matches!(
            state.run_slash_command("/provider codex"),
            Action::ActivateCodex
        ));
        assert_eq!(state.selected_model_name(), "claude:haiku");
        state.switch_to_codex();
        assert_eq!(state.selected_model_name(), "gpt-5.6-sol");
        state.run_slash_command("/model");
        let overlay = state.overlay_view().expect("Codex model picker");
        let model_lines = overlay
            .lines
            .iter()
            .filter(|line| !line.text.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(model_lines.len(), 2);
        assert!(model_lines.iter().all(|line| line.text.contains("GPT")));
    }

    #[test]
    fn provider_picker_switches_between_claude_and_codex() {
        let mut state = provider_picker_state();

        state.run_slash_command("/provider");
        let overlay = state.overlay_view().expect("provider picker");
        assert!(overlay.lines.is_empty());
        let slider = overlay.slider.expect("provider steps");
        assert_eq!(
            slider.efforts,
            ["Claude · 연결됨 · 사용 중", "Codex · 연결됨 · 미사용"]
        );
        assert_eq!(slider.selected, 0);

        state.handle_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(
            state
                .overlay_view()
                .expect("picker")
                .slider
                .expect("provider steps")
                .selected,
            1
        );

        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Enter)),
            Action::ActivateCodex
        ));
        assert!(state.pending.is_none());
        assert_eq!(state.selected_model_name(), "claude:sonnet");

        // Esc leaves the provider untouched, and a click on a row picks the
        // runtime the same way Enter does.
        state.run_slash_command("/provider");
        state.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(state.pending.is_none());

        state.switch_to_codex();
        state.run_slash_command("/provider");
        assert_eq!(
            state
                .overlay_view()
                .expect("picker")
                .slider
                .expect("provider steps")
                .selected,
            1
        );
        assert!(matches!(state.click_effort_step(0), Action::None));
        assert_eq!(state.selected_model_name(), "claude:sonnet");
    }

    #[test]
    fn late_mcp_toggle_results_do_not_modify_another_provider_picker() {
        let mut state = test_state();
        state.open_mcp_picker(
            vec![McpServerInfo::probe("browser", "unsupported", 2)],
            None,
        );

        assert!(!state.apply_mcp_enabled(
            SkillProvider::Claude,
            "browser",
            false,
            "late Claude result",
        ));
        assert!(
            state.overlay_view().expect("Codex MCP picker").lines[0]
                .text
                .starts_with("[x]")
        );

        assert!(state.apply_mcp_enabled(SkillProvider::Codex, "browser", false, "saved",));
        assert!(
            state.overlay_view().expect("Codex MCP picker").lines[0]
                .text
                .starts_with("[ ]")
        );
    }

    /// The company-PC case: Codex is switched off, so nothing in the picker,
    /// the command, or a later launch dials the app-server.
    #[test]
    fn switching_the_codex_connection_off_blocks_every_route_into_it() {
        let mut state = provider_picker_state();
        state.run_slash_command("/provider");
        state.handle_key(KeyEvent::from(KeyCode::Down));

        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Char(' '))),
            Action::PersistProviderConnection {
                key_path: CODEX_PROVIDER_KEY,
                connected: false,
                activate_codex: false,
            }
        ));
        let overlay = state.overlay_view().expect("picker stays open");
        assert_eq!(
            overlay.slider.expect("provider steps").efforts[1],
            "Codex · 연결 안 됨 · 미사용"
        );
        state.pending = None;

        // The command reconnects rather than failing: choosing Codex is what
        // connects it, and only then does the app-server get started.
        assert!(matches!(
            state.run_slash_command("/provider codex"),
            Action::PersistProviderConnection {
                key_path: CODEX_PROVIDER_KEY,
                connected: true,
                activate_codex: true,
            }
        ));
        assert_eq!(state.selected_model_name(), "claude:sonnet");

        // Already connected, so the next pick is a plain switch.
        state.run_slash_command("/provider");
        state.handle_key(KeyEvent::from(KeyCode::Down));
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Enter)),
            Action::ActivateCodex
        ));
    }

    /// A fresh install has no default: both rows are off, the first prompt opens
    /// the picker instead of guessing, and the typed text survives the detour.
    #[test]
    fn a_machine_with_no_connection_asks_before_it_sends_anything() {
        let mut state = provider_picker_state();
        state.claude_provider_enabled = false;
        state.codex_provider_enabled = false;

        state.prompt_for_provider_if_unconnected();
        assert!(state.provider_choice_pending);
        let overlay = state.overlay_view().expect("provider picker");
        assert_eq!(
            overlay.slider.expect("provider steps").efforts,
            [
                "Claude · 연결 안 됨 · 사용 중",
                "Codex · 연결 안 됨 · 미사용"
            ]
        );
        state.pending = None;

        state.editor.set_text("첫 질문");
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Enter)),
            Action::None
        ));
        assert_eq!(state.editor.text(), "첫 질문");
        assert!(state.overlay_view().is_some());

        // Enter on the Claude row connects it and switches in one keystroke.
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Enter)),
            Action::PersistProviderConnection {
                key_path: CLAUDE_PROVIDER_KEY,
                connected: true,
                activate_codex: false,
            }
        ));
        assert!(state.any_provider_connected());
        assert_eq!(state.selected_model_name(), "claude:sonnet");
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Enter)),
            Action::Submit(text) if text == "첫 질문"
        ));
    }

    /// Dropping the runtime in use hands the session to whatever is still
    /// connected, in either direction.
    #[test]
    fn disconnecting_the_live_runtime_hands_the_session_to_the_other_one() {
        let mut state = provider_picker_state();
        state.switch_to_codex();
        assert_eq!(state.selected_model_name(), "gpt-5.6-sol");

        state.run_slash_command("/provider");
        state.handle_key(KeyEvent::from(KeyCode::Down));
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Char(' '))),
            Action::PersistProviderConnection {
                key_path: CODEX_PROVIDER_KEY,
                connected: false,
                activate_codex: false,
            }
        ));
        assert_eq!(state.selected_model_name(), "claude:sonnet");
        assert!(!state.provider_choice_pending);
        state.pending = None;

        // The other direction: Claude is live, Codex is back, so dropping Claude
        // starts Codex rather than leaving the session with nothing.
        state.codex_provider_enabled = true;
        state.run_slash_command("/provider");
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Char(' '))),
            Action::PersistProviderConnection {
                key_path: CLAUDE_PROVIDER_KEY,
                connected: false,
                activate_codex: true,
            }
        ));

        // Dropping the last one leaves the session waiting for a pick again.
        state.pending = None;
        state.claude_provider_enabled = true;
        state.run_slash_command("/provider");
        state.handle_key(KeyEvent::from(KeyCode::Down));
        state.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        state.handle_key(KeyEvent::from(KeyCode::Up));
        state.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        assert!(!state.any_provider_connected());
        assert!(state.provider_choice_pending);
    }

    /// Built on the real constructor, then pinned to both runtimes connected so
    /// the developer's own saved settings cannot decide the test.
    fn provider_picker_state() -> AppState {
        let models = vec![
            test_model("gpt-5.6-sol", "GPT-5.6 Sol", true),
            test_model("claude:sonnet", "Claude Sonnet", false),
        ];
        let mut state = AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            models,
            "claude:sonnet",
            Some("high"),
        );
        state.claude_provider_enabled = true;
        state.codex_provider_enabled = true;
        state
    }

    #[test]
    fn codex_disconnect_falls_back_to_claude_without_closing_the_ui() {
        let models = vec![
            test_model("gpt-5.6-sol", "GPT-5.6 Sol", true),
            test_model("claude:sonnet", "Claude Sonnet", false),
        ];
        let mut state = AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            models,
            "gpt-5.6-sol",
            Some("high"),
        );
        state.busy = true;
        state.turn_id = Some("turn".to_owned());

        assert!(state.fallback_from_codex("app-server 연결이 종료되었습니다."));
        assert_eq!(state.selected_model_name(), "claude:sonnet");
        assert!(!state.busy);
        assert!(state.turn_id.is_none());
        assert!(state.committed.iter().any(|block| {
            block.title == "Codex 사용 불가" && block.body.contains("자동 전환했습니다")
        }));
    }

    #[test]
    fn shifted_model_navigation_stays_scoped_and_follows_catalog_order() {
        let mut codex = AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![
                test_model("gpt-5.6-sol", "GPT-5.6 Sol", true),
                test_model("claude:sonnet", "Claude Sonnet", false),
                test_model("gpt-5.6-terra", "GPT-5.6 Terra", false),
            ],
            "gpt-5.6-sol",
            Some("high"),
        );
        codex.move_selected_model(1);
        assert_eq!(codex.selected_model_name(), "gpt-5.6-terra");
        codex.move_selected_model(-1);
        assert_eq!(codex.selected_model_name(), "gpt-5.6-sol");

        let mut claude = AppState::new(
            "claude:thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![
                test_model("claude:fable", "Fable", false),
                test_model("claude:opus", "Opus", false),
                test_model("claude:sonnet", "Sonnet", true),
                test_model("claude:haiku", "Haiku", false),
            ],
            "claude:fable",
            Some("high"),
        );
        for (key, expected) in [
            (KeyCode::Down, "claude:opus"),
            (KeyCode::Down, "claude:sonnet"),
            (KeyCode::Up, "claude:opus"),
        ] {
            claude.handle_key(KeyEvent::new(key, KeyModifiers::SHIFT));
            assert_eq!(claude.selected_model_name(), expected);
        }
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
            Action::ReconnectMcp(None)
        ));
        assert!(matches!(
            state.run_slash_command("/mcp reconnect github"),
            Action::ReconnectMcp(Some(ref name)) if name == "github"
        ));
        assert!(matches!(
            state.run_slash_command("/mcp login github"),
            Action::McpLogin(ref name) if name == "github"
        ));
        assert!(matches!(state.run_slash_command("/connect"), Action::None));
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
                provider: SkillProvider::Codex,
                ref name,
                enabled: false
            } if name == "imagegen"
        ));
    }

    #[test]
    fn permissions_command_opens_claude_rules_and_keeps_codex_fixed() {
        let mut claude = AppState::new(
            "claude:thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![test_model("claude:sonnet", "Sonnet", true)],
            "claude:sonnet",
            Some("high"),
        );

        assert!(matches!(
            claude.run_slash_command("/permissions"),
            Action::OpenClaudePermissions(None)
        ));
        claude.open_claude_permissions(
            &json!({
                "rules": [{
                    "behavior": "allow",
                    "rule": "Read(./docs/**)",
                    "source": "projectSettings",
                    "mutable": true
                }],
                "directories": [],
                "denials": []
            }),
            None,
        );
        let panel = claude.overlay_view().expect("permission rules");
        assert_eq!(panel.title, "Permissions · Allow");
        assert!(panel.lines[0].text.contains("Read(./docs/**)"));
        assert!(panel.lines[0].text.contains("Project settings"));
        assert!(matches!(
            claude.run_slash_command("/permissions dont-ask"),
            Action::None
        ));

        let mut codex = test_state();
        assert!(matches!(
            codex.run_slash_command("/permissions"),
            Action::None
        ));
        assert!(
            codex
                .committed
                .last()
                .is_some_and(|block| block.body.contains("Full Access"))
        );
    }

    #[test]
    fn claude_permission_requests_show_the_provider_reason() {
        let mut state = test_state();
        assert!(matches!(
            state.begin_server_request(
                json!(91),
                "item/permissions/requestApproval",
                &json!({
                    "reason": "Claude가 외부 경로를 읽으려 합니다.",
                    "permissions": { "tool": "Read", "blockedPath": "D:/outside" }
                }),
            ),
            Action::None
        ));

        let overlay = state.overlay_view().expect("permission approval");
        assert_eq!(overlay.title, "추가 권한을 허용할까요?");
        assert!(
            overlay
                .lines
                .iter()
                .any(|line| line.text.contains("외부 경로"))
        );
    }

    #[test]
    fn claude_permission_prompts_only_offer_sdk_suggestions_persistently() {
        let mut state = test_state();
        state.begin_server_request(
            json!(92),
            "item/commandExecution/requestApproval",
            &json!({
                "claudePermission": true,
                "title": "Claude wants to run npm test",
                "command": "npm test"
            }),
        );
        let overlay = state.overlay_view().expect("Claude approval");
        assert_eq!(overlay.title, "Claude wants to run npm test");
        assert!(
            !overlay
                .lines
                .iter()
                .any(|line| line.text.contains("이 프로젝트에서 항상 허용"))
        );
        assert_eq!(overlay.hint, "↑↓ 선택   Enter 확정");

        let mut state = test_state();
        state.begin_server_request(
            json!(93),
            "item/commandExecution/requestApproval",
            &json!({
                "claudePermission": true,
                "command": "npm test",
                "persistentApprovalLabel": "이 프로젝트에서 항상 허용: Bash(npm test)"
            }),
        );
        let overlay = state.overlay_view().expect("Claude persistent approval");
        assert!(
            overlay.lines.iter().any(|line| {
                line.text == "이 프로젝트에서 항상 허용: Bash(npm test)"
            })
        );
        assert_eq!(overlay.hint, "↑↓ 선택   Enter 확정");
    }

    #[test]
    fn recently_denied_actions_retry_with_the_original_input_after_close() {
        let mut state = test_state();
        state.open_claude_permissions(
            &json!({
                "rules": [],
                "directories": [],
                "denials": [{
                    "tool": "Bash",
                    "reason": "classifier",
                    "input": { "command": "git push" }
                }]
            }),
            None,
        );
        for _ in 0..4 {
            assert!(matches!(
                state.handle_key(KeyEvent::from(KeyCode::Right)),
                Action::None
            ));
        }
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Char('r'))),
            Action::None
        ));
        let overlay = state.overlay_view().expect("marked retry");
        assert!(overlay.lines[0].text.starts_with("↻ Bash"));

        match state.handle_key(KeyEvent::from(KeyCode::Esc)) {
            Action::RetryClaudePermissionDenial { tool, input } => {
                assert_eq!(tool, "Bash");
                assert_eq!(
                    input.get("command").and_then(Value::as_str),
                    Some("git push")
                );
            }
            _ => panic!("closing the panel should resume the marked denial"),
        }
    }

    #[test]
    fn claude_permission_rules_add_and_remove_in_the_selected_scope() {
        let mut state = test_state();
        state.open_claude_permissions(
            &json!({ "rules": [], "directories": [], "denials": [] }),
            None,
        );
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Char('a'))),
            Action::None
        ));
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Enter)),
            Action::None
        ));
        for ch in "Read".chars() {
            assert!(matches!(
                state.handle_key(KeyEvent::from(KeyCode::Char(ch))),
                Action::None
            ));
        }
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Enter)),
            Action::UpdateClaudePermission {
                action: "add",
                ref behavior,
                ref value,
                ref destination,
            } if behavior == "allow" && value == "Read" && destination == "project"
        ));

        state.open_claude_permissions(
            &json!({
                "rules": [{
                    "behavior": "allow",
                    "rule": "Read",
                    "source": "project",
                    "mutable": true
                }],
                "directories": [],
                "denials": []
            }),
            None,
        );
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Char('d'))),
            Action::UpdateClaudePermission {
                action: "remove",
                ref behavior,
                ref value,
                ref destination,
            } if behavior == "allow" && value == "Read" && destination == "project"
        ));
    }

    #[test]
    fn auto_mode_requires_the_same_session_opt_in_as_claude_code() {
        let mut state = AppState::new(
            "claude:thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![test_model("claude:sonnet", "Sonnet", true)],
            "claude:sonnet",
            Some("high"),
        );
        state.claude_permission_mode = ClaudePermissionMode::Plan;

        assert!(matches!(state.cycle_claude_permission_mode(), Action::None));
        assert_eq!(
            state.overlay_view().expect("auto opt-in").title,
            "Enable auto mode?"
        );
        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Enter)),
            Action::SetClaudePermissionMode(ClaudePermissionMode::Auto)
        ));
        assert_eq!(
            state.claude_permission_mode(),
            Some(ClaudePermissionMode::Auto)
        );
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

    /// The badge walks Claude's modes in order and stops short of a bypass when
    /// settings forbid it.
    #[test]
    fn the_permission_badge_cycles_the_available_claude_modes() {
        let mut state = AppState::new(
            "claude:thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![test_model("claude:sonnet", "Sonnet", true)],
            "claude:sonnet",
            Some("high"),
        );
        state.claude_permission_mode = ClaudePermissionMode::Default;
        state.bypass_permissions_allowed = false;
        state.claude_auto_mode_confirmed = true;

        assert_eq!(
            state.claude_permission_mode(),
            Some(ClaudePermissionMode::Default)
        );
        let walked = std::iter::repeat_with(|| match state.cycle_claude_permission_mode() {
            Action::SetClaudePermissionMode(mode) => mode,
            _ => panic!("confirmed auto mode should switch immediately"),
        })
        .take(4)
        .collect::<Vec<_>>();
        assert_eq!(
            walked,
            [
                ClaudePermissionMode::AcceptEdits,
                ClaudePermissionMode::Plan,
                ClaudePermissionMode::Auto,
                ClaudePermissionMode::Default,
            ]
        );

        state.bypass_permissions_allowed = true;
        state.claude_permission_mode = ClaudePermissionMode::Default;
        let walked = std::iter::repeat_with(|| match state.cycle_claude_permission_mode() {
            Action::SetClaudePermissionMode(mode) => mode,
            _ => panic!("confirmed auto mode should switch immediately"),
        })
        .take(5)
        .collect::<Vec<_>>();
        assert_eq!(
            walked,
            [
                ClaudePermissionMode::AcceptEdits,
                ClaudePermissionMode::Plan,
                ClaudePermissionMode::BypassPermissions,
                ClaudePermissionMode::Auto,
                ClaudePermissionMode::Default,
            ]
        );
    }

    #[test]
    fn unsupported_claude_models_hide_and_leave_auto_mode() {
        let mut model = test_model("claude:haiku", "Haiku", true);
        model.supports_auto_mode = false;
        let mut state = AppState::new(
            "claude:thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![model],
            "claude:haiku",
            Some("high"),
        );
        state.claude_permission_mode = ClaudePermissionMode::Auto;

        state.replace_models(state.models.clone());

        assert_eq!(
            state.claude_permission_mode(),
            Some(ClaudePermissionMode::Default)
        );
        state.open_claude_permission_picker();
        let choices = state
            .overlay_view()
            .and_then(|view| view.slider)
            .expect("permission choices")
            .efforts;
        assert!(!choices.iter().any(|choice| choice == "Auto mode"));
    }

    /// Shift+Tab is how the CLI cycles these, so it cycles them here too — and
    /// the badge is the only feedback, with no notice flashing under the composer.
    #[test]
    fn both_shift_tab_encodings_cycle_claude_permissions_without_queueing() {
        for (key, busy) in [(KeyCode::BackTab, false), (KeyCode::Tab, true)] {
            let mut state = AppState::new(
                "claude:thread".to_owned(),
                "cwd".to_owned(),
                "account".to_owned(),
                vec![test_model("claude:sonnet", "Sonnet", true)],
                "claude:sonnet",
                Some("high"),
            );
            state.claude_permission_mode = ClaudePermissionMode::Default;
            state.busy = busy;
            if busy {
                state.editor.set_text("queued prompt");
            }

            let action = state.handle_key(KeyEvent::new(key, KeyModifiers::SHIFT));
            assert!(matches!(
                action,
                Action::SetClaudePermissionMode(ClaudePermissionMode::AcceptEdits)
            ));
            assert_eq!(
                state.claude_permission_mode(),
                Some(ClaudePermissionMode::AcceptEdits)
            );
            assert!(state.queued_prompts.is_empty());
            assert!(state.composer_notice.is_none());
        }
    }

    /// The vibe badge answers to Alt+V as well as to a click, mid-turn included.
    #[test]
    fn alt_v_cycles_the_vibe_preset_during_a_turn() {
        let mut state = busy_state_with_live_turn();
        while state.vibe_mode() != VibeMode::Vibe {
            state.cycle_vibe_mode();
        }

        let action = state.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT));

        assert_eq!(state.vibe_mode(), VibeMode::SuperVibe);
        assert!(matches!(
            action,
            Action::PersistVibeDisplayModes {
                vibe: VibeMode::SuperVibe,
                ..
            }
        ));
    }

    /// Codex has no such modes, so the badge stays off its composer rule and the
    /// turn carries nothing.
    #[test]
    fn a_codex_thread_has_no_permission_mode() {
        assert_eq!(test_state().claude_permission_mode(), None);
    }

    /// Claude reports mode values in mixed case, so wire parsing must preserve them.
    #[test]
    fn permission_modes_are_read_back_case_insensitively() {
        assert_eq!(
            ClaudePermissionMode::from_wire("acceptedits"),
            Some(ClaudePermissionMode::AcceptEdits)
        );
        assert_eq!(
            ClaudePermissionMode::from_wire("bypasspermissions"),
            Some(ClaudePermissionMode::BypassPermissions)
        );
        assert_eq!(
            ClaudePermissionMode::from_wire("plan"),
            Some(ClaudePermissionMode::Plan)
        );
        assert_eq!(
            ClaudePermissionMode::from_wire("auto"),
            Some(ClaudePermissionMode::Auto)
        );
        assert_eq!(
            ClaudePermissionMode::from_wire("dontask"),
            Some(ClaudePermissionMode::DontAsk)
        );
        assert_eq!(ClaudePermissionMode::from_wire("nonsense"), None);
    }

    /// A new session has no turn to report usage yet, so the gauge has to come
    /// from the catalog window alone. Claude models used to publish no window at
    /// all, which left the status line blank until — and after — the first reply.
    #[test]
    fn a_fresh_session_shows_the_window_before_the_first_turn() {
        let mut model = test_model("claude:opus[1m]", "Opus 5", true);
        model.context_window = Some(1_000_000);
        let state = AppState::new(
            "claude:thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![model],
            "claude:opus[1m]",
            Some("high"),
        );

        assert_eq!(
            state.status_line().context.as_deref(),
            Some("ctx: 0k/1000k (0%)")
        );
    }

    #[test]
    fn context_status_line_shows_used_window_and_percent_in_k() {
        let mut state = test_state();
        state.context_tokens = 100_000;
        state.context_window = Some(1_000_000);

        assert_eq!(
            state.status_line().context.as_deref(),
            Some("ctx: 100k/1000k (10%)")
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

        // 1M input on sol ($5) + the 1M delta on terra ($2).
        assert_eq!(state.composer_mode().cost.as_deref(), Some("$7.00"));
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
