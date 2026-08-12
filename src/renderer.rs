use std::{
    borrow::Cow,
    collections::HashSet,
    env, fs,
    io::{BufWriter, Stdout, Write, stdout},
    ops::Range,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, MoveDown, MoveTo, MoveToColumn, MoveUp, Show, position as cursor_position},
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size as terminal_size,
    },
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    editor::{ATTACHMENT_PLACEHOLDER, Editor},
    selection::{
        CellPosition, CellRange, CopyLine, Selection, SelectionFinish, extract_text,
        selected_char_count, selection_chunks,
    },
    state::{DiffDisplayMode, ShellDisplayMode},
    syntax::{self, SyntaxKind},
    theme::{self, Rgb, ThemeKind},
};

/// Which of the two ways of putting the transcript on screen is in use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RenderMode {
    /// The alternate screen, with the transcript scrolled by us. Every frame is
    /// a whole screen whose last rows are the composer and the status line, so
    /// no amount of scrolling can push them out of view.
    Fullscreen,
    /// The main screen: the transcript is handed to the terminal's own
    /// scrollback and the composer trails whatever was printed last. Scrolling
    /// is the terminal's, so the composer scrolls away with everything else —
    /// the cost of keeping the transcript selectable and copyable as real text.
    Inline,
}

impl RenderMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fullscreen" | "full" | "alt" | "pinned" => Some(Self::Fullscreen),
            "inline" | "classic" | "default" | "main" => Some(Self::Inline),
            _ => None,
        }
    }
}

/// Resolves the renderer, most explicit source first: the command line, then the
/// environment, then the saved choice, then pinned-composer as the default.
pub fn load_render_mode(cli_override: Option<&str>) -> Result<RenderMode> {
    if let Some(value) = cli_override {
        return RenderMode::parse(value)
            .with_context(|| format!("지원하지 않는 렌더러입니다: {value}"));
    }
    if let Some(value) = env::var("DEVEZ_VIBE_RENDERER")
        .ok()
        .filter(|v| !v.is_empty())
    {
        return RenderMode::parse(&value)
            .with_context(|| format!("DEVEZ_VIBE_RENDERER 값을 알 수 없습니다: {value}"));
    }
    Ok(fs::read_to_string(render_mode_file())
        .ok()
        .and_then(|value| RenderMode::parse(&value))
        .unwrap_or(RenderMode::Fullscreen))
}

fn render_mode_file() -> PathBuf {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("DevezVibe")
        .join("renderer.txt")
}

#[derive(Clone, Copy)]
pub enum BlockKind {
    Welcome,
    Update,
    User,
    Assistant,
    /// Earlier assistant updates from one completed turn, folded behind a
    /// disclosure row while the final answer stays visible below it.
    ProgressGroup,
    Reasoning,
    /// A `turn/plan/updated` snapshot. Its body is the encoded plan: `└ ` rows
    /// are the explanation, `✔ `/`▸ `/`□ ` rows are done/in-progress/pending
    /// steps. See [`plan_lines`].
    #[allow(dead_code)]
    Plan,
    Tool,
    FileChange,
    Diff,
    ModelChange,
    Warning,
    Error,
    System,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AssistantPhase {
    #[default]
    Unknown,
    Commentary,
    FinalAnswer,
}

#[derive(Clone)]
pub struct Block {
    id: u64,
    pub kind: BlockKind,
    pub title: String,
    pub body: String,
    children: Vec<Block>,
    assistant_phase: AssistantPhase,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderHandoffBlock {
    pub id: u64,
    pub kind: &'static str,
    pub title: String,
    pub body: String,
}

impl ProviderHandoffBlock {
    pub fn from_block(block: &Block) -> Option<Self> {
        let kind = match block.kind {
            BlockKind::User => "user",
            BlockKind::Assistant => "assistant",
            BlockKind::Reasoning => "reasoning",
            BlockKind::Plan => "plan",
            BlockKind::Tool => "tool",
            BlockKind::FileChange | BlockKind::Diff => "file_change",
            BlockKind::Welcome
            | BlockKind::Update
            | BlockKind::ProgressGroup
            | BlockKind::ModelChange
            | BlockKind::Warning
            | BlockKind::Error
            | BlockKind::System => return None,
        };
        Some(Self {
            id: block.id,
            kind,
            title: block.title.clone(),
            body: block.body.clone(),
        })
    }
}

#[derive(Clone, Copy)]
pub struct LiveBlockView<'a> {
    pub block: &'a Block,
    pub revision: u64,
}

static NEXT_BLOCK_ID: AtomicU64 = AtomicU64::new(1);
static CHAT_LAYOUT: AtomicBool = AtomicBool::new(true);
/// Cells a chat bubble keeps between its edge and its text, per side.
const CHAT_BUBBLE_PADDING: usize = 1;
/// Extra cell that keeps the right edge visibly clear after terminal painting.
const CHAT_BUBBLE_RIGHT_GAP: usize = 1;
/// History stays readable while sitting a little behind the prompt text.
const HISTORY_LABEL_MUTED_BLEND: u8 = 96;

impl Block {
    pub fn new(kind: BlockKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            id: NEXT_BLOCK_ID.fetch_add(1, Ordering::Relaxed),
            kind,
            title: title.into(),
            body: body.into(),
            children: Vec::new(),
            assistant_phase: AssistantPhase::Unknown,
        }
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn adopt_id(&mut self, source: &Self) {
        self.id = source.id;
    }

    pub fn with_assistant_phase(mut self, phase: AssistantPhase) -> Self {
        self.assistant_phase = phase;
        self
    }

    pub const fn assistant_phase(&self) -> AssistantPhase {
        self.assistant_phase
    }

    pub fn adopt_assistant_phase(&mut self, source: &Self) {
        if self.assistant_phase == AssistantPhase::Unknown {
            self.assistant_phase = source.assistant_phase;
        }
    }

    pub fn shell_group(kind: BlockKind, title: impl Into<String>, children: Vec<Block>) -> Self {
        let child_id = children.first().map(Block::id);
        let mut block = Self::new(kind, title, "");
        if let Some(child_id) = child_id {
            block.id = child_id;
        }
        block.children = children;
        block
    }

    pub fn file_change_group(title: impl Into<String>, children: Vec<Block>) -> Self {
        let child_id = children.first().map(Block::id);
        let mut block = Self::new(BlockKind::FileChange, title, "");
        if let Some(child_id) = child_id {
            block.id = child_id;
        }
        block.children = children;
        block
    }

    pub fn progress_group(children: Vec<Block>) -> Self {
        let count = children.len();
        let child_id = children.first().map(Block::id);
        let mut block = Self::new(
            BlockKind::ProgressGroup,
            format!("{HISTORY_TITLE} · {count}"),
            "",
        );
        if let Some(child_id) = child_id {
            block.id = child_id;
        }
        block.children = children;
        block
    }

    pub fn children(&self) -> &[Block] {
        &self.children
    }

    /// Credits come last so the variable-length list survives the round trip
    /// through [`BlockKind::Welcome`]'s newline-delimited body.
    pub fn welcome(
        provider: &str,
        plan: &str,
        cwd: &str,
        account: &str,
        credits: &[String],
    ) -> Self {
        let mut body = format!("{provider}\n{plan}\n{cwd}\n{account}");
        for line in credits {
            body.push('\n');
            body.push_str(line);
        }
        Self::new(BlockKind::Welcome, "DEVEZ VIBE", body)
    }
}

pub struct OverlayView<'a> {
    pub title: String,
    pub lines: Vec<OverlayLine>,
    /// Painted after `lines`, centred and coloured by the renderer. Structured
    /// rather than pre-formatted because each tier carries its own tone.
    pub slider: Option<EffortSlider>,
    pub hint: String,
    /// Whether the panel carries a `✕` just inside its top-right corner. What the
    /// user opened, they may close; what the server is waiting on, they may not.
    pub closable: bool,
    pub style: OverlayStyle,
    pub input: Option<&'a Editor>,
    pub input_label: &'static str,
    pub input_placeholder: &'static str,
}

/// The `Faster ─── Smarter` track a reasoning effort is picked on.
pub struct EffortSlider {
    pub efforts: Vec<String>,
    pub selected: usize,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OverlayStyle {
    Panel,
    CompactPanel,
    Picker,
    /// A question the server is waiting on: a picker-style box with a bold
    /// prompt and numbered options. The first row is the prompt and the last is
    /// the row that hands the turn back to the composer, which is why the rule
    /// above it is the renderer's to draw.
    Question,
}

#[derive(Clone)]
pub struct OverlayLine {
    pub text: String,
    pub selected: bool,
    pub muted: bool,
}

pub struct WelcomeView {
    pub provider: String,
    pub plan: String,
    /// Reset-credit rows: a summary first, then one line per credit.
    pub credits: Vec<String>,
    pub cwd: String,
    pub account: String,
}

pub struct SuggestionView {
    pub command: String,
    pub description: String,
    pub selected: bool,
    pub category: Option<String>,
    pub panel_title: &'static str,
    pub hint: Option<String>,
}

pub struct StatusLineView {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context: Option<String>,
    pub five_hour_percent: Option<u8>,
    /// Countdown to the 5h window reset (`3h 33m`); absent when the provider
    /// reports no 5h window at all.
    pub five_hour_remaining: Option<String>,
    pub weekly_percent: Option<u8>,
    pub notice: Option<String>,
}

/// Internal footer marker used when the user disables the status line entirely.
pub(crate) const HIDDEN_STATUS_LINE: &str = "\0";

/// How prominently a composer mode badge is painted on the composer top rule.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ModeAccent {
    #[allow(dead_code)]
    Calm,
    #[allow(dead_code)]
    Safe,
    Danger,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VibeTone {
    Off,
    On,
    Super,
}

/// The Claude permission mode badge, as the composer should paint it. Claude
/// Code gives each mode its own colour, so the badge carries one rather than
/// deriving it from a label the renderer would have to parse.
#[derive(Clone, PartialEq, Eq)]
pub struct PermissionBadge {
    pub label: String,
    pub tone: PermissionTone,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PermissionTone {
    Neutral,
    AcceptEdits,
    Plan,
    Auto,
    Bypass,
}

pub struct ComposerMode {
    /// Current Git branch, shown as a display-only composer badge.
    pub branch: Option<String>,
    pub vibe_mode: String,
    pub vibe_tone: VibeTone,
    #[allow(dead_code)]
    pub label: String,
    #[allow(dead_code)]
    pub accent: ModeAccent,
    pub model: String,
    pub response_length: String,
    pub fast_mode: bool,
    /// Claude's permission mode, painted where a Codex thread shows Fast. `None`
    /// for the runtimes that have no such mode.
    pub claude_permission: Option<PermissionBadge>,
    #[allow(dead_code)]
    pub effort: String,
    pub shell_display_mode: String,
    pub diff_display_mode: String,
    /// What the thread is estimated to have cost so far. Absent before the first
    /// turn reports usage, and whenever the model has no published rate.
    #[allow(dead_code)]
    pub cost: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStepStatus {
    Completed,
    InProgress,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanStep {
    pub text: String,
    pub status: PlanStepStatus,
    pub started_at: Option<Instant>,
    pub elapsed: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanSummary {
    pub explanation: Option<String>,
    pub steps: Vec<PlanStep>,
    pub expanded: bool,
    pub started_at: Instant,
    pub elapsed: Option<Duration>,
}

/// One provider subagent that is still running, including in the background.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentView {
    /// The parent `Task` tool-use id: what the transcript panel is keyed on.
    pub id: String,
    pub name: String,
    pub description: String,
    pub tool: String,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrationItemState {
    Active,
    Inactive,
    Pending,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationItemView {
    pub name: String,
    pub state: IntegrationItemState,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderIntegrationView {
    pub provider: String,
    pub enabled: bool,
    pub active: bool,
    pub mcp_expanded: bool,
    pub plugins_expanded: bool,
    pub mcp: Option<Vec<IntegrationItemView>>,
    pub plugins: Option<Vec<IntegrationItemView>>,
    pub mcp_error: Option<String>,
    pub plugin_error: Option<String>,
}

pub struct View<'a> {
    pub live_blocks: Vec<LiveBlockView<'a>>,
    pub overlay: Option<OverlayView<'a>>,
    /// A persistent right-hand panel, available only to the fullscreen renderer.
    pub plan_summary: Option<&'a PlanSummary>,
    /// The newly folded progress group and the share of its rows still visible.
    pub response_collapse: Option<(u64, f32)>,
    /// Progress records are disclosure rows only in Super Vibe. Other presets
    /// render their child responses exactly as ordinary transcript blocks.
    pub fold_progress_groups: bool,
    /// Whether the current turn is still active, so an in-progress plan row may animate.
    pub plan_active: bool,
    /// A one-shot frame shimmer started by a plan creation or update.
    pub plan_shimmer_phase: Option<f32>,
    /// The effort fixed when the active request started, used for plan shimmer.
    pub plan_effort: Option<&'a str>,
    pub editor: &'a Editor,
    pub composer_images: &'a [String],
    pub queued_prompts: Vec<String>,
    /// Running subagents shown under the status line.
    pub subagents: Vec<SubagentView>,
    pub composer_placeholder: &'a str,
    pub welcome: Option<WelcomeView>,
    pub suggestions: Vec<SuggestionView>,
    pub activity: Option<String>,
    /// The active turn's model. A transient activity notice leaves this empty
    /// so it uses the ordinary foreground colour.
    pub activity_model: Option<String>,
    /// Where the `Working` shimmer is in its sweep, `0.0..1.0`.
    pub activity_phase: f32,
    /// The turn is active, but its first assistant text has not appeared yet.
    pub waiting_for_response: bool,
    /// How many characters at the end of the streamed text are still arriving,
    /// so they can be brought up from the background instead of appearing at
    /// full strength the instant they land.
    pub stream_fade_tail: usize,
    /// Where the compaction progress block is in its slower trip, `0.0..1.0`.
    pub activity_progress_phase: f32,
    pub footer: String,
    pub status_line: Option<StatusLineView>,
    pub composer_notice: Option<String>,
    pub composer_mode: Option<ComposerMode>,
    pub chat_layout: bool,
    pub shell_display_mode: ShellDisplayMode,
    pub diff_display_mode: DiffDisplayMode,
    /// The docked right-hand side panel's width, or `None` while it is closed.
    pub side_panel_width: Option<usize>,
    pub side_panel_prompts_expanded: bool,
    pub side_panel_integrations: Vec<ProviderIntegrationView>,
}

pub struct AnimationView<'a> {
    pub activity: Option<String>,
    pub activity_model: Option<String>,
    pub activity_phase: f32,
    pub waiting_for_response: bool,
    pub activity_progress_phase: f32,
    pub plan_summary: Option<&'a PlanSummary>,
    pub plan_active: bool,
    pub plan_shimmer_phase: Option<f32>,
    pub plan_effort: Option<&'a str>,
    pub composer_notice: Option<&'a str>,
    pub composer_mode: Option<ComposerMode>,
}

pub struct TerminalSession {
    mode: RenderMode,
}

fn enter_fullscreen(out: &mut impl Write) -> std::io::Result<()> {
    // Mode 1007 turns wheel movement into Up/Down key sequences on the alternate
    // screen. Save the user's setting, then disable only that translation.
    // Mouse capture is fullscreen-only: it takes drag selection away from the
    // terminal, which is the point here but would be a loss inline, where the
    // transcript is ordinary scrollback the terminal knows how to select.
    execute!(out, EnterAlternateScreen, Print("\x1b[?1007s"))?;
    disable_alternate_scroll(out)
}

/// Prevents terminals from translating a wheel tick into an Up/Down key while
/// the alternate screen is active.
fn disable_alternate_scroll(out: &mut impl Write) -> std::io::Result<()> {
    execute!(out, Print("\x1b[?1007l"))
}

fn leave_fullscreen(out: &mut impl Write) -> std::io::Result<()> {
    execute!(out, Print("\x1b[?1007r"), LeaveAlternateScreen)
}

impl TerminalSession {
    pub fn enter(mode: RenderMode) -> Result<Self> {
        enable_raw_mode()?;
        execute!(stdout(), EnableBracketedPaste, Show)?;
        if mode == RenderMode::Fullscreen {
            enter_fullscreen(&mut stdout())?;
            // Crossterm uses WinAPI console input modes on Windows and ANSI
            // mouse-reporting sequences elsewhere. Raw escape sequences alone
            // do not enable Windows console mouse events.
            execute!(stdout(), EnableMouseCapture)?;
            // Keep this after mouse capture as well. Some terminals update
            // private mouse modes together, which otherwise lets a wheel tick
            // reach the composer as an Up/Down history key.
            disable_alternate_scroll(&mut stdout())?;
        }
        Ok(Self { mode })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.mode == RenderMode::Fullscreen {
            // Undone before the colour reset below, so the OSC restores land on
            // the main screen where they are what the user keeps looking at.
            let _ = execute!(stdout(), DisableMouseCapture);
            let _ = leave_fullscreen(&mut stdout());
        }
        let _ = execute!(
            stdout(),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Print("\x1b]110\x07\x1b]111\x07\x1b]112\x07"),
            Show,
            DisableBracketedPaste
        );
        let _ = disable_raw_mode();
    }
}

/// One frame of escape sequences for a wide terminal runs to tens of kilobytes.
/// `std::io::stdout` buffers only a kilobyte at a time, so a single repaint left
/// through it as dozens of separate console writes; on Windows each one is a
/// round trip expensive enough to push a frame past its interval. Holding a whole
/// frame and handing it over in one write is what keeps the pace even.
const FRAME_BUFFER_BYTES: usize = 512 * 1024;

pub struct Renderer {
    out: BufWriter<Stdout>,
    mode: RenderMode,
    previous_lines: Vec<PaintLine>,
    cursor_line: usize,
    cursor_col: usize,
    cursor_shown: bool,
    /// The width conversation rows are laid out against. With the panel open
    /// this is narrower than the terminal.
    last_width: u16,
    /// The terminal's own width, which is what a painted frame spans.
    last_total_width: u16,
    last_height: u16,
    /// Transcript rows available in the last fullscreen frame. Prompt jumps use
    /// this to place their block at the top whenever enough history follows it.
    last_transcript_rows: usize,
    /// The wrapped transcript row shown at the top of the last fullscreen frame.
    /// Transcript drags use this stable coordinate while the viewport scrolls.
    last_transcript_start: usize,
    /// Screen row where that transcript window begins, below any fixed plan.
    last_transcript_screen_start: usize,
    /// Transcript height held across a History disclosure toggle. A short
    /// transcript would otherwise claim newly available rows when expanded and
    /// make its final line jump down even though the composer stayed fixed.
    history_view_rows_anchor: Option<usize>,
    /// Transcript start held for the render immediately after a prompt-hosted
    /// History toggle, keeping that prompt on the pointer's screen row.
    history_view_start_anchor: Option<usize>,
    theme: ThemeKind,
    history: Vec<Block>,
    /// Rows the transcript is held back from its newest end. Zero follows the
    /// live output. Fullscreen only: inline scrolling belongs to the terminal.
    scroll_back: usize,
    /// The transcript already wrapped for `wrapped_width`. Fullscreen repaints
    /// the whole screen every keystroke, and re-wrapping the transcript each
    /// time would make typing cost O(transcript).
    wrapped: Vec<PaintLine>,
    wrapped_width: u16,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    chat_layout: bool,
    expanded_tools: HashSet<u64>,
    response_collapse: Option<(u64, f32)>,
    fold_progress_groups: bool,
    /// Wrapped transcript rows owned by folded progress records. Double-click
    /// word selection is intentionally disabled only inside these rows.
    progress_group_rows: Vec<Range<usize>>,
    hovered_tool: Option<u64>,
    painted_hovered_tool: Option<u64>,
    /// The clickable piece of chrome under the pointer, and the one the screen
    /// was last painted with, so only the rows whose highlight moved repaint.
    hovered_pick: Option<Pick>,
    painted_hovered_pick: Option<Pick>,
    selection: Selection,
    last_click: Option<(CellPosition, Instant)>,
    /// Where the composer's prompt text was last painted, so a drag over it can
    /// be turned back into the characters it covered.
    composer_selection: Option<ComposerSelection>,
    /// The last painted prompt rows, retained in both render modes so vertical
    /// arrows can traverse visual wraps before falling back to prompt history.
    composer_navigation_layout: Option<ComposerLayout>,
    painted_selection: Option<CellRange>,
    painted_frame: Option<CellFrame>,
    /// The docked panel's geometry for the frame now on screen, so animation
    /// repaints can restore the panel cells they would otherwise blank.
    side_panel: Option<SidePanelLayout>,
    /// What the docked panel shows on its own surface in the frame now on
    /// screen: today the plan summary, moved out of the transcript.
    side_panel_content: Vec<PaintLine>,
    /// Context and usage readings moved out of the composer status row while
    /// the panel is visible.
    side_panel_footer: Vec<PaintLine>,
    /// Which surface the live drag belongs to. The transcript and the panel are
    /// two separate columns of text, so a drag stays inside the one it started
    /// on instead of running across the border between them.
    selection_in_panel: bool,
    /// Transcript selections use wrapped-history row coordinates rather than
    /// screen rows, allowing a live drag to continue across wheel scrolling.
    selection_in_transcript: bool,
    live_frame_cache: Option<LiveFrameCache>,
    animation_activity_row: Option<usize>,
    animation_response_bullet_row: Option<usize>,
    animation_plan_rows: usize,
    #[cfg(test)]
    live_cache_rebuilds: usize,
}

/// Where the composer's prompt text sits on screen, one entry per painted row in
/// paint order. This is what lets a drag over the composer be answered in
/// characters rather than cells.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ComposerLayout {
    rows: Vec<ComposerRowLayout>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComposerRowLayout {
    /// Column the row's first glyph is painted at.
    start_column: usize,
    /// Composer cursor positions at the row's left and right edges. Explicit
    /// newlines consume the gap between adjacent rows; visual wraps do not.
    start: usize,
    end: usize,
    glyphs: Vec<ComposerGlyph>,
}

/// One painted glyph of the prompt: the cells it fills, and the composer
/// characters behind it. An image label and a collapsed-paste summary both stand
/// for more than they show, so the span is a range rather than one index, and
/// padding that stands for nothing carries an empty span.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ComposerGlyph {
    width: usize,
    span: Range<usize>,
}

fn composer_cursor_column(row: &ComposerRowLayout, cursor: usize) -> usize {
    let mut column = row.start_column;
    for glyph in &row.glyphs {
        if cursor <= glyph.span.start {
            return column;
        }
        column += glyph.width;
        if cursor < glyph.span.end {
            return column;
        }
    }
    column
}

fn composer_cursor_at_column(row: &ComposerRowLayout, target: usize) -> usize {
    if target <= row.start_column {
        return row.start;
    }
    let mut column = row.start_column;
    for glyph in &row.glyphs {
        let end = column + glyph.width;
        if target < end {
            if glyph.span.start == glyph.span.end {
                return glyph.span.start;
            }
            return if (target - column) * 2 < glyph.width {
                glyph.span.start
            } else {
                glyph.span.end
            };
        }
        column = end;
    }
    row.end
}

struct ComposerSelection {
    /// Screen row the composer's first prompt row was painted on.
    first_row: usize,
    layout: ComposerLayout,
}

/// One resolved terminal cell. The last terminal column may carry a style, but
/// never a glyph: emitting a glyph there risks terminal autowrap.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cell {
    glyph: String,
    style: CellStyle,
    continuation: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellStyle {
    foreground: Option<Rgb>,
    background: Option<Rgb>,
    bold: bool,
    italic: bool,
    underlined: bool,
    crossed_out: bool,
}

impl CellStyle {
    const fn plain() -> Self {
        Self {
            foreground: None,
            background: None,
            bold: false,
            italic: false,
            underlined: false,
            crossed_out: false,
        }
    }
}

impl Cell {
    fn blank(style: CellStyle) -> Self {
        Self {
            glyph: " ".to_owned(),
            style,
            continuation: false,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct CellFrame {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
}

struct LiveFrameCache {
    blocks: Vec<(u64, u64)>,
    width: u16,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    chat_layout: bool,
    expanded_tools: HashSet<u64>,
    lines: Vec<PaintLine>,
}

/// A terminal cell may be one visible character plus following zero-width
/// selectors/combining marks. Keeping them together matters for text-style
/// emoji such as `⚡︎`: terminals render it in one cell, and splitting the
/// variation selector makes the frame's later cells drift from the screen.
fn display_units(text: &str) -> Vec<&str> {
    let mut units = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if UnicodeWidthChar::width(ch).unwrap_or(0) > 0 {
            if let Some(previous) = start.replace(index) {
                units.push(&text[previous..index]);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(start) = start {
        units.push(&text[start..]);
    }
    units
}

fn terminal_unit_width(unit: &str) -> usize {
    UnicodeWidthStr::width(unit)
}

/// 터미널 기본 탭 정지 간격.
const TAB_STOP: usize = 8;

/// 탭은 폭 0이라 셀 격자에서는 자리를 차지하지 않지만, 그대로 터미널에 나가면
/// 커서만 다음 탭 정지까지 건너뛴다. 그 구간은 배경이 칠해지지 않아 검은 띠로
/// 남고 뒤따르는 셀이 통째로 밀려 선택 하이라이트까지 어긋나므로, 폭을 재기
/// 전에 공백으로 펼쳐 둔다.
fn expand_tabs(text: &str) -> Cow<'_, str> {
    if !text.contains('\t') {
        return Cow::Borrowed(text);
    }
    let mut expanded = String::with_capacity(text.len());
    let mut column = 0;
    for ch in text.chars() {
        if ch == '\t' {
            let advance = TAB_STOP - column % TAB_STOP;
            expanded.extend(std::iter::repeat_n(' ', advance));
            column += advance;
        } else {
            expanded.push(ch);
            column += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    Cow::Owned(expanded)
}

/// 셀에 남은 제어문자는 터미널 커서를 움직여 격자를 깨뜨린다.
fn without_control_characters(unit: &str) -> Cow<'_, str> {
    if unit.chars().any(char::is_control) {
        Cow::Owned(unit.chars().filter(|ch| !ch.is_control()).collect())
    } else {
        Cow::Borrowed(unit)
    }
}

impl CellFrame {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::blank(CellStyle::plain()); width.saturating_mul(height)],
        }
    }

    fn cell(&self, column: usize, row: usize) -> &Cell {
        &self.cells[row * self.width + column]
    }

    fn cell_mut(&mut self, column: usize, row: usize) -> &mut Cell {
        &mut self.cells[row * self.width + column]
    }

    fn fill(&mut self, left: usize, top: usize, right: usize, bottom: usize, style: CellStyle) {
        for row in top.min(self.height)..bottom.min(self.height) {
            for column in left.min(self.width)..right.min(self.width) {
                *self.cell_mut(column, row) = Cell::blank(style);
            }
        }
    }

    fn write(&mut self, mut column: usize, row: usize, text: &str, style: CellStyle) {
        if row >= self.height {
            return;
        }
        for unit in display_units(text) {
            let glyph_width = terminal_unit_width(unit);
            let unit = without_control_characters(unit);
            let unit = unit.as_ref();
            if glyph_width == 0 {
                if !unit.is_empty() && column > 0 && column - 1 < self.width {
                    self.cell_mut(column - 1, row).glyph.push_str(unit);
                }
                continue;
            }
            // Keep the physical final column for an erase operation, never a
            // printed glyph, so terminal autowrap cannot spill into row + 1.
            if column + glyph_width >= self.width {
                break;
            }
            *self.cell_mut(column, row) = Cell {
                glyph: unit.to_owned(),
                style,
                continuation: false,
            };
            for offset in 1..glyph_width {
                *self.cell_mut(column + offset, row) = Cell {
                    glyph: String::new(),
                    style,
                    continuation: true,
                };
            }
            column += glyph_width;
        }
    }
}

/// The three widths Alt+P cycles the panel through before it closes again.
pub(crate) const SIDE_PANEL_WIDTHS: [usize; 3] = [48, 60, 72];
/// Keeps the completed MCP/Plugin panel implementation available without
/// connecting it to session startup or the visible side panel.
pub(crate) const SIDE_PANEL_INTEGRATIONS_CONNECTED: bool = false;
const SIDE_PANEL_GAP: usize = 1;
const SIDE_PANEL_MIN_MAIN_WIDTH: usize = 44;
/// Keeps section rules present without competing with the content above them.
const SIDE_PANEL_DIVIDER_BLEND: u8 = 48;
/// A quiet neutral lift that remains visible on the panel's secondary surface.
const SIDE_PANEL_HOVER_BLEND: u8 = 24;
/// A stronger lift makes the floating transcript control visibly interactive.
const SCROLL_TO_BOTTOM_HOVER_BLEND: u8 = 48;

fn devez_layout_signal(main_width: u16) -> String {
    format!("\x1b]777;devez-layout-v1;{main_width}\x07")
}

/// Where the docked panel sits once the terminal is wide enough to carry it
/// without squeezing the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SidePanelLayout {
    main_width: usize,
    panel_left: usize,
    panel_width: usize,
}

impl SidePanelLayout {
    const HORIZONTAL_PADDING: usize = 2;

    fn content_left(self) -> usize {
        self.panel_left + Self::HORIZONTAL_PADDING
    }

    fn content_width(self) -> usize {
        self.panel_width
            .saturating_sub(2 * Self::HORIZONTAL_PADDING)
    }
}

/// Leaves the conversation enough room to stay readable before docking the
/// requested panel width at the right edge; a terminal too narrow for it keeps
/// the panel shut rather than shrinking it to something else the user did not
/// ask for.
fn side_panel_layout(total_width: u16, panel_width: usize) -> Option<SidePanelLayout> {
    let total = usize::from(total_width);
    let reserved = SIDE_PANEL_GAP + panel_width;
    (total >= SIDE_PANEL_MIN_MAIN_WIDTH + reserved).then(|| {
        let main_width = total - reserved;
        SidePanelLayout {
            main_width,
            panel_left: main_width + SIDE_PANEL_GAP,
            panel_width,
        }
    })
}

fn side_panel_background_style() -> CellStyle {
    CellStyle {
        background: Some(theme::palette().hover_bg),
        ..CellStyle::plain()
    }
}

fn side_panel_hover_background() -> Rgb {
    let palette = theme::palette();
    blend(palette.hover_bg, palette.foreground, SIDE_PANEL_HOVER_BLEND)
}

fn scroll_to_bottom_background(hovered: bool) -> Rgb {
    let palette = theme::palette();
    if hovered {
        blend(
            palette.hover_bg,
            palette.foreground,
            SCROLL_TO_BOTTOM_HOVER_BLEND,
        )
    } else {
        palette.hover_bg
    }
}

/// Paints one already-laid-out row of panel content at the panel's own left
/// inset. The row is drawn through a scratch frame of the content width so the
/// ordinary line painter can be reused without teaching it a column offset.
fn paint_panel_content_row(
    frame: &mut CellFrame,
    layout: SidePanelLayout,
    frame_row: usize,
    line: &PaintLine,
    selected_columns: Option<Range<usize>>,
    hovered_columns: Option<Range<usize>>,
) {
    let content_width = layout.content_width();
    if content_width == 0 {
        return;
    }
    // `CellFrame::write` keeps the final column clear for autowrap safety, so the
    // scratch frame carries one spare column the panel never copies back.
    let mut scratch = CellFrame::new(content_width + 1, 1);
    paint_line_into_frame(
        &mut scratch,
        0,
        line,
        selected_columns.clone(),
        None,
        Some(content_width + 1),
    );
    for column in 0..content_width {
        let mut cell = scratch.cell(column, 0).clone();
        let selected = range_overlaps(selected_columns.as_ref(), column, column + 1);
        let hovered = !selected && range_overlaps(hovered_columns.as_ref(), column, column + 1);
        if hovered {
            cell.style.background = Some(side_panel_hover_background());
        } else {
            cell.style
                .background
                .get_or_insert(theme::palette().hover_bg);
        }
        *frame.cell_mut(layout.content_left() + column, frame_row) = cell;
    }
}

/// Paints one row of the docked panel as a borderless theme surface. The first
/// and last rows stay empty to provide the same outer breathing room as the
/// reference panel without drawing a rule or corner around it.
fn paint_side_panel_row_into_frame(
    frame: &mut CellFrame,
    layout: SidePanelLayout,
    frame_row: usize,
    global_row: usize,
    rows: usize,
    content: &[PaintLine],
    selection: Option<CellRange>,
    footer: &[PaintLine],
    hovered_pick: Option<&Pick>,
) {
    if frame_row >= frame.height || rows == 0 {
        return;
    }
    frame.fill(
        layout.panel_left,
        frame_row,
        layout.panel_left + layout.panel_width,
        frame_row + 1,
        side_panel_background_style(),
    );
    if global_row == 0 || global_row + 1 == rows {
        return;
    }
    let footer_start = rows.saturating_sub(footer.len().saturating_add(1));
    if global_row >= footer_start
        && global_row + 1 < rows
        && let Some(line) = footer.get(global_row - footer_start)
    {
        let hovered_columns = Renderer::hover_columns(line, None, hovered_pick);
        paint_panel_content_row(frame, layout, frame_row, line, None, hovered_columns);
        return;
    }
    // Row zero is the panel's empty top inset, so content starts one row below it.
    if let Some(line) = content.get(global_row - 1) {
        let selected_columns =
            selection.and_then(|range| selection_columns_for_line(line, range, global_row - 1));
        let hovered_columns = Renderer::hover_columns(line, None, hovered_pick);
        paint_panel_content_row(
            frame,
            layout,
            frame_row,
            line,
            selected_columns,
            hovered_columns,
        );
    }
}

#[cfg(test)]
fn paint_side_panel_into_frame(
    frame: &mut CellFrame,
    layout: SidePanelLayout,
    rows: usize,
    content: &[PaintLine],
    selection: Option<CellRange>,
) {
    paint_side_panel_into_frame_with_footer(frame, layout, rows, content, selection, &[], None);
}

fn paint_side_panel_into_frame_with_footer(
    frame: &mut CellFrame,
    layout: SidePanelLayout,
    rows: usize,
    content: &[PaintLine],
    selection: Option<CellRange>,
    footer: &[PaintLine],
    hovered_pick: Option<&Pick>,
) {
    for row in 0..rows.min(frame.height) {
        paint_side_panel_row_into_frame(
            frame,
            layout,
            row,
            row,
            rows,
            content,
            selection,
            footer,
            hovered_pick,
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SelectionResult {
    Copy(String),
    /// The cell a click landed on, column first: a tool heading answers to the
    /// row alone, but the clickable chrome is only ever part of a row.
    Click(u16, u16),
    None,
}

fn replace_history_block(history: &mut Vec<Block>, incoming: Block) -> bool {
    let Some(existing) = history
        .iter_mut()
        .find(|existing| existing.id() == incoming.id())
    else {
        history.push(incoming);
        return false;
    };
    *existing = incoming;
    true
}

fn merge_history_block(history: &mut Vec<Block>, incoming: Block) -> bool {
    if !matches!(incoming.kind, BlockKind::ProgressGroup) {
        return replace_history_block(history, incoming);
    }
    let child_ids = incoming
        .children()
        .iter()
        .map(Block::id)
        .collect::<HashSet<_>>();
    let insertion = history
        .iter()
        .position(|block| child_ids.contains(&block.id()) || block.id() == incoming.id());
    let Some(insertion) = insertion else {
        history.push(incoming);
        return false;
    };
    history.retain(|block| !child_ids.contains(&block.id()) && block.id() != incoming.id());
    history.insert(insertion.min(history.len()), incoming);
    true
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewportRelation {
    Before,
    Overlapping,
    After,
}

fn viewport_relation(changed: Range<usize>, viewport: Range<usize>) -> ViewportRelation {
    if changed.end <= viewport.start {
        ViewportRelation::Before
    } else if changed.start >= viewport.end {
        ViewportRelation::After
    } else {
        ViewportRelation::Overlapping
    }
}

impl Renderer {
    pub fn new(selected_theme: ThemeKind, mode: RenderMode) -> Self {
        theme::set_current(selected_theme);
        Self {
            out: BufWriter::with_capacity(FRAME_BUFFER_BYTES, stdout()),
            mode,
            previous_lines: Vec::new(),
            cursor_line: 0,
            cursor_col: 0,
            cursor_shown: true,
            last_width: 0,
            last_total_width: 0,
            last_height: 0,
            last_transcript_rows: 0,
            last_transcript_start: 0,
            last_transcript_screen_start: 0,
            history_view_rows_anchor: None,
            history_view_start_anchor: None,
            theme: selected_theme,
            history: Vec::new(),
            scroll_back: 0,
            wrapped: Vec::new(),
            wrapped_width: 0,
            shell_display_mode: ShellDisplayMode::Collapse,
            diff_display_mode: DiffDisplayMode::Collapse,
            chat_layout: false,
            expanded_tools: HashSet::new(),
            response_collapse: None,
            fold_progress_groups: false,
            progress_group_rows: Vec::new(),
            hovered_tool: None,
            painted_hovered_tool: None,
            hovered_pick: None,
            painted_hovered_pick: None,
            selection: Selection::default(),
            last_click: None,
            composer_selection: None,
            composer_navigation_layout: None,
            painted_selection: None,
            painted_frame: None,
            side_panel: None,
            side_panel_content: Vec::new(),
            side_panel_footer: Vec::new(),
            selection_in_panel: false,
            selection_in_transcript: false,
            live_frame_cache: None,
            animation_activity_row: None,
            animation_response_bullet_row: None,
            animation_plan_rows: 0,
            #[cfg(test)]
            live_cache_rebuilds: 0,
        }
    }

    pub const fn mode(&self) -> RenderMode {
        self.mode
    }

    /// Provider runtimes cannot resume each other's native session IDs. This
    /// keeps the portable, user-visible part of the transcript available for a
    /// handoff while leaving welcome cards and local UI notices behind.
    pub fn provider_handoff_blocks(&self) -> Vec<ProviderHandoffBlock> {
        self.history
            .iter()
            .flat_map(|block| {
                if matches!(block.kind, BlockKind::ProgressGroup) {
                    block
                        .children()
                        .iter()
                        .filter_map(ProviderHandoffBlock::from_block)
                        .collect::<Vec<_>>()
                } else {
                    ProviderHandoffBlock::from_block(block)
                        .into_iter()
                        .collect()
                }
            })
            .collect()
    }

    pub fn last_history_block_id(&self) -> u64 {
        self.history.iter().map(Block::id).max().unwrap_or_default()
    }

    /// Moves the transcript view by `delta` rows, positive being back into
    /// history. Reports whether the view actually moved, so a wheel spun at
    /// either end costs no repaint. The clamp against the bottom happens here;
    /// the clamp against the viewport height waits for `render`, which is the
    /// first place the live frame's height is known.
    pub fn scroll(&mut self, delta: isize) -> bool {
        if self.mode != RenderMode::Fullscreen {
            return false;
        }
        let target = self
            .scroll_back
            .saturating_add_signed(delta)
            .min(self.wrapped.len());
        let moved = target != self.scroll_back;
        self.scroll_back = target;
        if moved {
            self.history_view_rows_anchor = None;
            self.history_view_start_anchor = None;
        }
        let preserve_drag = self.selection_in_transcript && self.selection.is_dragging();
        let cleared = !preserve_drag && self.clear_selection();
        moved || cleared
    }

    /// Returns the fullscreen transcript to its newest position. Inline mode
    /// leaves scrolling to the terminal's own scrollback.
    pub fn scroll_to_bottom(&mut self) -> bool {
        if self.mode != RenderMode::Fullscreen || self.scroll_back == 0 {
            return false;
        }
        self.scroll_back = 0;
        self.history_view_rows_anchor = None;
        self.history_view_start_anchor = None;
        self.clear_selection();
        true
    }

    /// Places a previously sent prompt at the top of the fullscreen transcript,
    /// or as high as the remaining rows allow when it is near the newest end.
    pub fn scroll_to_prompt(&mut self, block_id: u64) -> bool {
        if self.mode != RenderMode::Fullscreen || self.last_transcript_rows == 0 {
            return false;
        }
        let mut target_start = 0;
        let mut found = false;
        for block in visible_transcript_blocks(
            &self.history,
            self.shell_display_mode,
            self.diff_display_mode,
        ) {
            if block.id() == block_id {
                found = true;
                break;
            }
            target_start += self.history_block_lines(block, self.last_width).len();
        }
        if !found {
            return false;
        }
        let max_back = self.wrapped.len().saturating_sub(self.last_transcript_rows);
        let target = max_back.saturating_sub(target_start.min(max_back));
        let moved = target != self.scroll_back;
        self.scroll_back = target;
        if moved {
            self.history_view_rows_anchor = None;
            self.history_view_start_anchor = None;
            self.clear_selection();
        }
        moved
    }

    fn scroll_to_bottom_control(&self, width: u16) -> Option<PaintLine> {
        if self.mode != RenderMode::Fullscreen || self.scroll_back == 0 {
            return None;
        }
        let text = " Scroll to bottom (Ctrl+↓) ";
        let start = usize::from(width).saturating_sub(UnicodeWidthStr::width(text)) / 2;
        Some(PaintLine {
            prefix: " ".repeat(start),
            prefix_tone: Tone::Plain,
            text: text.to_owned(),
            tone: Tone::ScrollToBottom,
            bold: false,
            tool_heading: None,
            pick: Some(PickRegions::span(
                start,
                start + UnicodeWidthStr::width(text),
                Pick::ScrollToBottom,
            )),
            tail: Vec::new(),
        })
    }

    /// Rows currently visible in the transcript viewport.
    pub fn page_rows(&self) -> isize {
        self.last_transcript_rows.max(1) as isize
    }

    pub fn clear_screen(&mut self) -> Result<()> {
        self.history.clear();
        self.wrapped.clear();
        self.wrapped_width = 0;
        self.scroll_back = 0;
        self.expanded_tools.clear();
        self.response_collapse = None;
        self.progress_group_rows.clear();
        self.hovered_tool = None;
        self.painted_hovered_tool = None;
        self.hovered_pick = None;
        self.painted_hovered_pick = None;
        self.selection.clear();
        self.selection_in_panel = false;
        self.selection_in_transcript = false;
        self.painted_selection = None;
        self.composer_navigation_layout = None;
        self.painted_frame = None;
        self.live_frame_cache = None;
        self.animation_activity_row = None;
        self.animation_response_bullet_row = None;
        self.animation_plan_rows = 0;
        self.last_transcript_rows = 0;
        self.history_view_rows_anchor = None;
        self.history_view_start_anchor = None;
        self.apply_terminal_theme()?;
        self.reset_screen()
    }

    pub fn toggle_tool_at(&mut self, row: u16) -> bool {
        if self.mode != RenderMode::Fullscreen {
            return false;
        }
        let Some(id) = self
            .previous_lines
            .get(row as usize)
            .and_then(|line| line.tool_heading)
        else {
            return false;
        };

        self.toggle_tool(id)
    }

    pub fn toggle_tool(&mut self, id: u64) -> bool {
        if self.mode != RenderMode::Fullscreen {
            return false;
        }

        let history_toggle = self
            .history
            .iter()
            .any(|block| block.id() == id && matches!(block.kind, BlockKind::ProgressGroup));
        let hosted_history_toggle = history_toggle && self.prompt_for_progress_group(id).is_some();
        self.history_view_rows_anchor =
            (history_toggle && !hosted_history_toggle && self.scroll_back == 0)
                .then_some(self.last_transcript_rows);
        self.history_view_start_anchor =
            hosted_history_toggle.then_some(self.last_transcript_start);

        if !self.expanded_tools.remove(&id) {
            self.expanded_tools.insert(id);
        }
        let old_len = self.wrapped.len();
        self.rewrap(self.last_width.max(20));
        let delta = self.wrapped.len() as isize - old_len as isize;
        if self.scroll_back > 0 {
            self.scroll_back = self.scroll_back.saturating_add_signed(delta);
        }
        true
    }

    /// What the cell under a click stands for, if anything: the mode and fast
    /// badges on the composer rule, or the model and effort readings on the
    /// status line. Fullscreen only, which is the only mode that captures mouse
    /// events at all.
    pub fn pick_at(&self, column: u16, row: u16) -> Option<Pick> {
        if self.mode != RenderMode::Fullscreen {
            return None;
        }
        if self.column_is_in_panel(column) {
            let layout = self.side_panel?;
            let content_row = usize::from(row).checked_sub(1)?;
            let content_column = usize::from(column).checked_sub(layout.content_left())?;
            if content_column >= layout.content_width() {
                return None;
            }
            return self
                .side_panel_content
                .get(content_row)?
                .pick
                .as_ref()?
                .at(content_column);
        }
        self.previous_lines
            .get(row as usize)?
            .pick
            .as_ref()?
            .at(usize::from(column))
    }

    /// Tracks what the pointer is over: a tool heading, or a clickable piece of
    /// chrome. Everything that answers to a click lights up under the pointer, so
    /// what is clickable is discoverable without a legend.
    pub fn hover_at(&mut self, column: u16, row: u16) -> bool {
        if self.mode != RenderMode::Fullscreen {
            return false;
        }
        let hovered = self.previous_lines.get(row as usize).and_then(|line| {
            let start = UnicodeWidthStr::width(line.prefix.as_str());
            let end = start + UnicodeWidthStr::width(line.text.as_str());
            (usize::from(column) >= start && usize::from(column) < end)
                .then_some(line.tool_heading)
                .flatten()
        });
        let pick = self.pick_at(column, row);
        let changed = hovered != self.hovered_tool || pick != self.hovered_pick;
        self.hovered_tool = hovered;
        self.hovered_pick = pick;
        changed
    }

    /// The columns a row lights up when the pointer is on it: the heading's own
    /// text, or the span the hovered piece of chrome was painted across.
    fn hover_columns(
        line: &PaintLine,
        hovered_tool: Option<u64>,
        hovered_pick: Option<&Pick>,
    ) -> Option<Range<usize>> {
        if line.tool_heading.is_some() && line.tool_heading == hovered_tool {
            let start = UnicodeWidthStr::width(line.prefix.as_str());
            return Some(start..start + UnicodeWidthStr::width(line.text.as_str()));
        }
        let hovered_pick = hovered_pick?;
        let columns = line.pick.as_ref()?.columns_of(hovered_pick)?;
        if matches!(hovered_pick, Pick::Effort(_)) {
            let mut start = UnicodeWidthStr::width(line.prefix.as_str())
                + UnicodeWidthStr::width(line.text.as_str());
            for span in &line.tail {
                let end = start + UnicodeWidthStr::width(span.text.as_str());
                if span.bold
                    && start >= columns.start
                    && end <= columns.end
                    && span.text.starts_with("│ ")
                    && span.text.ends_with(" │")
                {
                    return Some(start + 2..end.saturating_sub(2));
                }
                start = end;
            }
        }
        // History owns the prompt's whole painted background, including the
        // trailing padding that contains no glyphs. Other controls still stop
        // at their last painted character.
        if matches!(hovered_pick, Pick::History(_)) {
            Some(columns)
        } else {
            Some(columns.start..columns.end.min(painted_line_width(line)))
        }
    }

    /// A live drag keeps the surface it began on, so leaving the transcript for
    /// the panel mid-drag extends the original selection instead of jumping.
    fn selection_target_is_panel(&self, column: u16) -> bool {
        if self.selection.is_dragging() {
            return self.selection_in_panel;
        }
        self.column_is_in_panel(column)
    }

    pub fn begin_selection(&mut self, column: u16, row: u16) -> bool {
        self.selection_in_panel = self.column_is_in_panel(column);
        self.selection_in_transcript =
            !self.selection_in_panel && self.transcript_row_at_screen(usize::from(row)).is_some();
        let Some(point) = self.selection_point(column, row) else {
            return false;
        };
        self.selection.begin(point);
        true
    }

    pub fn update_selection(&mut self, column: u16, row: u16) -> bool {
        let Some(point) = self.selection_point(column, row) else {
            return false;
        };
        self.selection.update(point)
    }

    pub fn finish_selection(&mut self, column: u16, row: u16) -> SelectionResult {
        let Some(point) = self.selection_point(column, row) else {
            return SelectionResult::None;
        };
        let in_panel = self.selection_target_is_panel(column);
        match self.selection.finish(point) {
            SelectionFinish::Copy(range) => {
                let text = extract_text(&self.copy_lines(), range);
                if text.trim().is_empty() {
                    SelectionResult::None
                } else {
                    SelectionResult::Copy(text)
                }
            }
            // The panel carries no clickable chrome, and its cells are not the
            // transcript's, so a bare click there must not resolve to a pick.
            SelectionFinish::Click(_) if in_panel => SelectionResult::None,
            SelectionFinish::Click(cell) => SelectionResult::Click(cell.column, row),
            SelectionFinish::None => SelectionResult::None,
        }
    }

    /// Selects and returns the word under a second click on the same cell.
    pub fn double_click_word(&mut self, column: u16, row: u16) -> Option<String> {
        const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

        let point = self.selection_point(column, row)?;
        if matches!(self.pick_at(column, row), Some(Pick::History(_)))
            || self
                .progress_group_rows
                .iter()
                .any(|range| range.contains(&point.row))
        {
            self.last_click = None;
            return None;
        }
        let now = Instant::now();
        let is_double_click =
            self.last_click
                .replace((point, now))
                .is_some_and(|(previous, clicked_at)| {
                    previous.row == point.row
                        && previous.column.abs_diff(point.column) <= 1
                        && now.duration_since(clicked_at) <= DOUBLE_CLICK_WINDOW
                });
        if !is_double_click {
            return None;
        }
        self.last_click = None;

        let lines = self.copy_lines();
        let line = lines.get(point.row)?;
        let range = word_range_at(line, usize::from(point.column))?;
        self.selection.set_range(CellRange {
            start: CellPosition {
                row: point.row,
                column: u16::try_from(range.start).ok()?,
            },
            end: CellPosition {
                row: point.row,
                column: u16::try_from(range.end.saturating_sub(1)).ok()?,
            },
        });
        let text = extract_text(&lines, self.selection.range()?);
        (!text.is_empty()).then_some(text)
    }

    pub fn clear_selection(&mut self) -> bool {
        self.selection_in_panel = false;
        self.selection_in_transcript = false;
        self.selection.clear()
    }

    /// Returns the active transcript selection without changing it. Keyboard
    /// copy uses this before clearing the highlight for the next key event.
    pub fn selected_text(&self) -> Option<String> {
        let range = self.selection.range()?;
        let text = extract_text(&self.copy_lines(), range);
        (!text.is_empty()).then_some(text)
    }

    /// The composer characters the active drag covers, as a range into the text
    /// the composer is showing. Backspace and Delete answer to this before they
    /// answer to the cursor, the way a selection does in any other editor. `None`
    /// unless the drag actually landed on prompt text.
    pub fn composer_selection_range(&self) -> Option<Range<usize>> {
        if self.selection_in_transcript || self.selection_in_panel {
            return None;
        }
        let range = self.selection.range()?;
        let composer = self.composer_selection.as_ref()?;
        let mut start = usize::MAX;
        let mut end = 0;
        for (offset, row) in composer.layout.rows.iter().enumerate() {
            let width =
                row.start_column + row.glyphs.iter().map(|glyph| glyph.width).sum::<usize>();
            let Some(columns) = range.columns_for_row(composer.first_row + offset, width) else {
                continue;
            };
            let mut column = row.start_column;
            for glyph in &row.glyphs {
                let selected = column < columns.end && column + glyph.width > columns.start;
                if selected && glyph.span.start < glyph.span.end {
                    start = start.min(glyph.span.start);
                    end = end.max(glyph.span.end);
                }
                column += glyph.width;
            }
        }
        (start < end).then_some(start..end)
    }

    /// Maps a click on a visible composer row to the closest safe cursor
    /// boundary. Clicking blank space clamps to that row instead of selecting
    /// terminal padding, and wide glyphs can be approached from either side.
    pub fn composer_cursor_position(&self, column: u16, row: u16) -> Option<usize> {
        let composer = self.composer_selection.as_ref()?;
        let offset = usize::from(row).checked_sub(composer.first_row)?;
        let row = composer.layout.rows.get(offset)?;
        let clicked = usize::from(column);
        if clicked <= row.start_column {
            return Some(row.start);
        }
        let mut glyph_column = row.start_column;
        for glyph in &row.glyphs {
            let glyph_end = glyph_column + glyph.width;
            if clicked < glyph_end {
                if glyph.span.start == glyph.span.end {
                    return Some(glyph.span.start);
                }
                let right_half = glyph.width > 1 && (clicked - glyph_column) * 2 >= glyph.width;
                return Some(if right_half {
                    glyph.span.end
                } else {
                    glyph.span.start
                });
            }
            glyph_column = glyph_end;
        }
        Some(row.end)
    }

    /// Resolves an Up/Down key against the visual rows from the last paint.
    /// `None` means the cursor is already at that edge and history may handle it.
    pub fn composer_vertical_cursor_position(&self, cursor: usize, delta: isize) -> Option<usize> {
        let layout = self.composer_navigation_layout.as_ref()?;
        let (row_index, row) = layout
            .rows
            .iter()
            .enumerate()
            .rev()
            .find(|(_, row)| row.start <= cursor && cursor <= row.end)?;
        let target_index = row_index.checked_add_signed(delta)?;
        let target = layout.rows.get(target_index)?;
        let desired_column = composer_cursor_column(row, cursor);
        Some(composer_cursor_at_column(target, desired_column))
    }

    #[cfg(test)]
    pub(crate) fn set_composer_navigation_layout_for_test(&mut self, editor: &Editor, width: u16) {
        let (_, _, _, layout) = input_lines(editor, &[], width, "", "", None, None);
        self.composer_navigation_layout = Some(layout);
    }

    fn reconcile_selection(&mut self, lines: &[PaintLine], plan_rows: usize) {
        let Some(range) = self.selection.range() else {
            return;
        };
        // Wrapped transcript coordinates do not move with the viewport. A wheel
        // scroll therefore changes screen rows without changing selected text.
        if self.selection_in_transcript {
            return;
        }
        // A panel selection indexes the panel's own content, which this frame's
        // transcript rows say nothing about.
        if self.selection_in_panel {
            return;
        }
        let changed = (range.start.row..=range.end.row).any(|row| {
            let Some((previous, current)) = self.previous_lines.get(row).zip(lines.get(row)) else {
                return true;
            };
            previous != current
                && (row >= plan_rows || plan_row_requires_full_repaint(previous, current))
        });
        if changed {
            self.selection.clear();
        }
    }

    /// An animation may repaint every other row while a drag keeps this row
    /// still. Panel selections use a different row grid and survive their
    /// ordinary full-frame paint path unchanged.
    fn animation_row_is_selected(&self, row: usize) -> bool {
        !self.selection_in_panel
            && self.selection.range().is_some_and(|range| {
                let row = if self.selection_in_transcript {
                    let Some(row) = self.transcript_row_at_screen(row) else {
                        return false;
                    };
                    row
                } else {
                    row
                };
                range.start.row <= row && row <= range.end.row
            })
    }

    /// True when the column belongs to the docked panel rather than the
    /// transcript. The gap between them counts as the panel's, so a drag that
    /// starts a hair left of the border still reads as a panel drag.
    fn column_is_in_panel(&self, column: u16) -> bool {
        self.side_panel
            .is_some_and(|layout| usize::from(column) >= layout.main_width)
    }

    /// Maps a screen cell onto the panel's own content grid: one row per content
    /// line below the top inset, and columns measured from the panel's left inset.
    fn panel_selection_point(&self, column: u16, row: u16) -> Option<CellPosition> {
        let layout = self.side_panel?;
        if self.side_panel_content.is_empty() || row == 0 {
            return None;
        }
        let content_row = usize::from(row) - 1;
        let content_row = content_row.min(self.side_panel_content.len().saturating_sub(1));
        let line = self.side_panel_content.get(content_row)?;
        let width = painted_line_width(line).min(layout.content_width());
        if width == 0 {
            return None;
        }
        let column = usize::from(column).saturating_sub(layout.content_left());
        let column = column.min(width - 1);
        Some(CellPosition {
            column: u16::try_from(column).ok()?,
            row: content_row,
        })
    }

    fn transcript_row_at_screen(&self, row: usize) -> Option<usize> {
        let first = self.last_transcript_screen_start;
        let offset = row.checked_sub(first)?;
        if offset >= self.last_transcript_rows {
            return None;
        }
        let transcript_row = self.last_transcript_start + offset;
        (transcript_row < self.wrapped.len()).then_some(transcript_row)
    }

    fn selection_point(&self, column: u16, row: u16) -> Option<CellPosition> {
        if self.mode != RenderMode::Fullscreen || self.previous_lines.is_empty() {
            return None;
        }
        if self.selection_target_is_panel(column) {
            return self.panel_selection_point(column, row);
        }
        let screen_row = if self.selection_in_transcript {
            let first = self.last_transcript_screen_start;
            if self.last_transcript_rows == 0 {
                return None;
            }
            let last = first + self.last_transcript_rows.saturating_sub(1);
            usize::from(row).clamp(first, last)
        } else {
            usize::from(row).min(self.previous_lines.len().saturating_sub(1))
        };
        let line = &self.previous_lines[screen_row];
        if matches!(
            line.tone,
            Tone::AssistantBubbleHalf | Tone::UserPromptPadding
        ) {
            return None;
        }
        let width = painted_line_width(line).max(
            line.pick
                .as_ref()
                .and_then(|picks| picks.0.iter().map(|(_, end, _)| *end).max())
                .unwrap_or_default(),
        );
        let column = if width == 0 {
            0
        } else {
            column.min(width.saturating_sub(1).min(u16::MAX as usize) as u16)
        };
        if line.tone == Tone::UserPrompt
            && usize::from(column) < UnicodeWidthStr::width(line.prefix.as_str())
        {
            return None;
        }
        let row = if self.selection_in_transcript {
            self.transcript_row_at_screen(screen_row)?
        } else {
            screen_row
        };
        Some(CellPosition { column, row })
    }

    fn copy_lines(&self) -> Vec<CopyLine> {
        let source = if self.selection_in_panel {
            &self.side_panel_content
        } else if self.selection_in_transcript {
            &self.wrapped
        } else {
            &self.previous_lines
        };
        source
            .iter()
            .map(|line| CopyLine {
                text: painted_line_text(line),
                join_next: copy_joins_next(line),
                marker_width: 0,
                prefix_width: UnicodeWidthStr::width(line.prefix.as_str()),
                content_columns: selectable_content_columns(line),
            })
            .collect()
    }

    pub fn set_theme(&mut self, selected_theme: ThemeKind) -> Result<()> {
        if self.theme == selected_theme {
            return Ok(());
        }
        self.erase_live()?;
        self.theme = selected_theme;
        theme::set_current(selected_theme);
        self.apply_terminal_theme()?;
        self.relayout()
    }

    /// Lays the transcript out again for the current cell grid. A window resize
    /// or a Ctrl+wheel font zoom leaves the terminal reflowing rows we wrapped
    /// ourselves, which splits them in places we never chose, so the whole
    /// screen is rebuilt at the new width instead. `render` repaints the live
    /// frame straight after, since `reset_screen` drops the frame bookkeeping.
    pub fn relayout(&mut self) -> Result<()> {
        if self.mode == RenderMode::Fullscreen {
            // Nothing to reprint: the transcript is ours, and the next `render`
            // rebuilds it at the new width. Dropping the cache is the whole job.
            self.wrapped_width = 0;
            return self.reset_screen();
        }
        self.reset_screen()?;
        let width = terminal_size().unwrap_or((100, 30)).0.max(20);
        // Moved out so the blocks can be printed without cloning the transcript;
        // put back whether or not a write fails partway.
        let history = std::mem::take(&mut self.history);
        let mut outcome = Ok(());
        for block in
            visible_transcript_blocks(&history, self.shell_display_mode, self.diff_display_mode)
        {
            let lines = self.history_block_lines(block, width);
            if let Err(error) = self.print_permanent(block, &lines) {
                outcome = Err(error);
                break;
            }
        }
        self.history = history;
        outcome
    }

    fn record_inline_history(&mut self, committed: &[Block]) {
        for block in committed.iter().cloned() {
            merge_history_block(&mut self.history, block);
        }
    }

    fn response_reveal_for(&self, block_id: u64) -> Option<f32> {
        self.response_collapse
            .filter(|(group_id, _)| *group_id == block_id)
            .map(|(_, reveal)| reveal)
    }

    fn progress_group_for_prompt(&self, prompt_id: u64) -> Option<&Block> {
        let mut current_prompt = None;
        for block in &self.history {
            if matches!(block.kind, BlockKind::User) {
                current_prompt = Some(block.id());
            } else if matches!(block.kind, BlockKind::ProgressGroup)
                && current_prompt == Some(prompt_id)
            {
                return Some(block);
            }
        }
        None
    }

    fn prompt_for_progress_group(&self, group_id: u64) -> Option<u64> {
        let mut prompt_id = None;
        for block in &self.history {
            if matches!(block.kind, BlockKind::User) {
                prompt_id = Some(block.id());
            } else if matches!(block.kind, BlockKind::ProgressGroup) && block.id() == group_id {
                return prompt_id;
            }
        }
        None
    }

    fn history_block_lines(&self, block: &Block, width: u16) -> Vec<PaintLine> {
        if matches!(block.kind, BlockKind::User)
            && self.fold_progress_groups
            && let Some(group) = self.progress_group_for_prompt(block.id())
        {
            let reveal = self.response_reveal_for(group.id());
            let expanded = self.expanded_tools.contains(&group.id())
                || reveal.is_some_and(|value| value > f32::EPSILON);
            return user_prompt_lines_with_history(
                block,
                width,
                Some((group.id(), &group.title, expanded)),
                self.chat_layout,
            );
        }
        if matches!(block.kind, BlockKind::ProgressGroup) && !self.fold_progress_groups {
            return block
                .children()
                .iter()
                .flat_map(|child| {
                    block_group_lines_at(
                        child,
                        width,
                        self.shell_display_mode,
                        self.diff_display_mode,
                        self.expanded_tools.contains(&child.id()),
                        None,
                    )
                })
                .collect();
        }
        if matches!(block.kind, BlockKind::ProgressGroup)
            && self.prompt_for_progress_group(block.id()).is_some()
        {
            return embedded_progress_group_lines(
                block,
                width,
                self.expanded_tools.contains(&block.id()),
                self.response_reveal_for(block.id()),
            );
        }
        block_group_lines_at(
            block,
            width,
            self.shell_display_mode,
            self.diff_display_mode,
            self.expanded_tools.contains(&block.id()),
            self.response_reveal_for(block.id()),
        )
    }

    fn remove_startup_update_from_history(&mut self) -> bool {
        let before = self.history.len();
        self.history.retain(|block| !is_startup_update(block));
        let removed = self.history.len() != before;
        if removed {
            self.wrapped_width = 0;
        }
        removed
    }

    fn reset_screen(&mut self) -> Result<()> {
        self.previous_lines.clear();
        self.painted_frame = None;
        self.hovered_tool = None;
        self.painted_hovered_tool = None;
        self.selection.clear();
        self.painted_selection = None;
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.cursor_shown = true;
        self.last_width = 0;
        self.last_height = 0;
        execute!(
            self.out,
            Print("\x1b]777;devez-copy-clear\x07"),
            Clear(ClearType::All),
            Clear(ClearType::Purge),
            MoveTo(0, 0),
            Show
        )?;
        Ok(())
    }

    fn apply_terminal_theme(&mut self) -> Result<()> {
        let palette = theme::palette();
        queue!(
            self.out,
            Print(format!(
                "\x1b]10;{}\x07\x1b]11;{}\x07\x1b]12;{}\x07",
                palette.foreground.hex(),
                palette.background.hex(),
                palette.foreground.hex()
            ))
        )?;
        self.out.flush()?;
        Ok(())
    }

    pub fn render(&mut self, committed: &[Block], view: View<'_>) -> Result<()> {
        CHAT_LAYOUT.store(view.chat_layout, Ordering::Relaxed);
        let response_collapse_changed = self.response_collapse != view.response_collapse;
        if response_collapse_changed {
            self.response_collapse = view.response_collapse;
            self.wrapped_width = 0;
            self.live_frame_cache = None;
            self.history_view_rows_anchor = None;
            self.history_view_start_anchor = None;
        }
        let committed_without_startup = view.plan_summary.is_some().then(|| {
            committed
                .iter()
                .filter(|block| !is_startup_update(block))
                .cloned()
                .collect::<Vec<_>>()
        });
        let committed = committed_without_startup.as_deref().unwrap_or(committed);
        let mode_changed = self.shell_display_mode != view.shell_display_mode
            || self.diff_display_mode != view.diff_display_mode
            || self.chat_layout != view.chat_layout
            || self.fold_progress_groups != view.fold_progress_groups;
        if mode_changed {
            self.shell_display_mode = view.shell_display_mode;
            self.diff_display_mode = view.diff_display_mode;
            self.chat_layout = view.chat_layout;
            self.fold_progress_groups = view.fold_progress_groups;
            self.wrapped_width = 0;
            self.history_view_rows_anchor = None;
            self.history_view_start_anchor = None;
        }
        let startup_update_removed =
            view.plan_summary.is_some() && self.remove_startup_update_from_history();
        if self.mode == RenderMode::Inline
            && (mode_changed || startup_update_removed)
            && !response_collapse_changed
        {
            self.relayout()?;
        }
        let (total_width, height) = terminal_size().unwrap_or((100, 30));
        let total_width = total_width.max(20);
        if total_width != self.last_total_width || height != self.last_height {
            self.history_view_rows_anchor = None;
            self.history_view_start_anchor = None;
        }
        // The panel is docked, not overlaid: every conversation row is laid out
        // against the narrowed main width so nothing runs under the panel.
        let side_panel = (self.mode == RenderMode::Fullscreen)
            .then_some(view.side_panel_width)
            .flatten()
            .and_then(|panel_width| side_panel_layout(total_width, panel_width));
        let mut status_line = view.status_line;
        let side_panel_footer = side_panel
            .map(|layout| move_context_to_side_panel(&mut status_line, layout.content_width()))
            .unwrap_or_default();
        if side_panel != self.side_panel {
            // Opening or closing moves every row's right edge, so the diff has
            // nothing reusable and the whole surface must be repainted.
            self.painted_frame = None;
            self.side_panel = side_panel;
        }
        let width = side_panel.map_or(total_width, |layout| layout.main_width as u16);
        if width != self.last_width {
            queue!(self.out, Print(devez_layout_signal(width)))?;
        }
        let frame_width = width;
        let live_lines =
            self.live_frame_lines(&view.live_blocks, frame_width, height.max(3) as usize);
        let status = StatusArea {
            fallback: view.footer,
            line: status_line,
            composer_notice: view.composer_notice,
            composer_mode: view.composer_mode,
        };
        let mut frame = if let Some(overlay) = view.overlay {
            overlay_frame_with_expansion(live_lines, overlay, view.welcome, status, frame_width)
        } else {
            normal_frame_with_expansion(
                live_lines,
                view.editor,
                view.composer_images,
                &view.queued_prompts,
                &view.subagents,
                view.composer_placeholder,
                view.welcome,
                &view.suggestions,
                view.activity.as_deref(),
                view.activity_model.as_deref(),
                view.activity_phase,
                view.activity_progress_phase,
                status,
                frame_width,
            )
        };
        let composer_navigation_layout = frame.composer_layout.clone();
        self.side_panel_footer = side_panel_footer;

        if self.mode == RenderMode::Fullscreen {
            return self.render_fullscreen(
                committed,
                frame,
                width,
                total_width,
                height.max(3),
                view.plan_summary,
                view.activity_phase,
                view.plan_active,
                view.plan_shimmer_phase,
                view.plan_effort,
                view.waiting_for_response,
                view.side_panel_prompts_expanded,
                &view.side_panel_integrations,
                view.stream_fade_tail,
            );
        }

        let max_live = height.max(3) as usize;
        let natural_rows = frame.lines.len().min(max_live);
        let hidden_thinking_merge = self.mode == RenderMode::Inline
            && hidden_thinking_merge_at_history_boundary(
                &self.history,
                committed,
                self.shell_display_mode,
                self.diff_display_mode,
            );
        let inline_history_replacement =
            self.mode == RenderMode::Inline && replaces_inline_history(&self.history, committed);
        let needs_full_repaint = self.previous_lines.is_empty()
            || self.last_width != width
            || self.last_height != height
            || !committed.is_empty()
            || mode_changed
            || hidden_thinking_merge
            || response_collapse_changed;
        if needs_full_repaint {
            self.erase_live()?;
            if hidden_thinking_merge || inline_history_replacement || response_collapse_changed {
                self.record_inline_history(committed);
                self.relayout()?;
            } else {
                for block in visible_transcript_blocks(
                    committed,
                    self.shell_display_mode,
                    self.diff_display_mode,
                ) {
                    let lines = self.history_block_lines(block, frame_width);
                    self.print_permanent(block, &lines)?;
                }
                self.record_inline_history(committed);
            }
            self.out.flush()?;
            let available_rows = cursor_position()
                .map(|(_, row)| height.saturating_sub(row).max(1) as usize)
                .unwrap_or(natural_rows);
            fit_frame(&mut frame, available_rows.max(natural_rows).min(max_live));
            self.print_frame_full(&frame)?;
        } else {
            fit_frame(
                &mut frame,
                self.previous_lines.len().max(natural_rows).min(max_live),
            );
            self.patch_frame(&frame)?;
        }

        self.previous_lines = frame.lines.clone();
        self.cursor_line = frame.cursor_line;
        self.cursor_col = frame.cursor_col;
        self.composer_navigation_layout = composer_navigation_layout;
        // Inline mode hands selection to the terminal, so nothing here maps a
        // drag back to the composer.
        self.composer_selection = None;
        self.animation_activity_row = None;
        self.animation_response_bullet_row = None;
        self.animation_plan_rows = 0;
        self.last_transcript_rows = 0;
        self.last_width = width;
        self.last_total_width = total_width;
        self.last_height = height;
        self.out.flush()?;
        Ok(())
    }

    fn live_frame_lines(
        &mut self,
        live: &[LiveBlockView<'_>],
        width: u16,
        max_rows: usize,
    ) -> Vec<PaintLine> {
        let blocks = live
            .iter()
            .map(|live| (live.block.id(), live.revision))
            .collect::<Vec<_>>();
        let reusable = self.live_frame_cache.as_ref().is_some_and(|cache| {
            cache.blocks == blocks
                && cache.width == width
                && cache.shell_display_mode == self.shell_display_mode
                && cache.diff_display_mode == self.diff_display_mode
                && cache.chat_layout == self.chat_layout
                && cache.expanded_tools == self.expanded_tools
        });
        if !reusable {
            #[cfg(test)]
            {
                self.live_cache_rebuilds += 1;
            }
            self.live_frame_cache = Some(LiveFrameCache {
                blocks,
                width,
                shell_display_mode: self.shell_display_mode,
                diff_display_mode: self.diff_display_mode,
                chat_layout: self.chat_layout,
                expanded_tools: self.expanded_tools.clone(),
                lines: render_live_block_lines(
                    live,
                    width,
                    &self.expanded_tools,
                    self.shell_display_mode,
                    self.diff_display_mode,
                ),
            });
        }
        let lines = &self
            .live_frame_cache
            .as_ref()
            .expect("live frame cache initialized")
            .lines;
        let start = lines.len().saturating_sub(max_rows);
        lines[start..].to_vec()
    }

    pub fn render_animation(&mut self, view: AnimationView<'_>) -> Result<bool> {
        if self.mode != RenderMode::Fullscreen
            || self.last_width == 0
            || self.previous_lines.is_empty()
            || self.painted_frame.is_none()
        {
            return Ok(false);
        }

        let mut updates = Vec::new();
        if let Some(activity) = view.activity.as_deref() {
            let Some(row) = self.animation_activity_row else {
                return Ok(false);
            };
            let mut activity_rows = activity_lines_with_progress(
                activity,
                view.activity_model.as_deref(),
                view.activity_phase,
                view.activity_progress_phase,
                self.last_width,
            );
            if activity_rows.len() != 1 {
                return Ok(false);
            }
            let mut line = activity_rows.pop().expect("one activity row");
            let composer_notice = view.composer_notice;
            if let Some(mode) = view.composer_mode.as_ref()
                && let Some(with_controls) = activity_line_with_composer_controls(
                    line.clone(),
                    mode,
                    composer_notice,
                    self.last_width,
                )
            {
                line = with_controls;
            }
            updates.push((row, line));
        } else if self.animation_activity_row.is_some() {
            return Ok(false);
        }

        if view.waiting_for_response
            && let Some(row) = self.animation_response_bullet_row
            && let Some(line) = self.previous_lines.get(row)
        {
            let mut line = line.clone();
            line.prefix_tone = waiting_response_bullet_tone(view.activity_phase);
            updates.push((row, line));
        }

        let plan_lines = view
            .plan_summary
            .map(|summary| {
                fixed_plan_summary_lines(
                    summary,
                    self.last_width,
                    view.activity_phase,
                    view.plan_active,
                    view.plan_shimmer_phase,
                    view.plan_effort,
                )
            })
            .unwrap_or_default();
        if plan_lines.len() != self.animation_plan_rows {
            return Ok(false);
        }
        updates.extend(plan_lines.into_iter().enumerate());
        let painted_height = self
            .painted_frame
            .as_ref()
            .map(|frame| frame.height)
            .unwrap_or(0);
        let mut rows = HashSet::new();
        if updates.is_empty()
            || updates.iter().any(|(row, _)| {
                *row >= self.previous_lines.len() || *row >= painted_height || !rows.insert(*row)
            })
        {
            return Ok(false);
        }
        updates.retain(|(row, _)| !self.animation_row_is_selected(*row));
        if updates.is_empty() {
            return Ok(true);
        }
        self.paint_animation_rows(&updates)?;
        for (row, line) in updates {
            self.previous_lines[row] = line;
        }
        Ok(true)
    }

    fn paint_animation_rows(&mut self, updates: &[(usize, PaintLine)]) -> Result<()> {
        let width = usize::from(self.last_total_width);
        let Some(painted) = self.painted_frame.as_ref() else {
            return Ok(());
        };
        if updates
            .iter()
            .any(|(row, _)| *row >= painted.height || *row >= self.previous_lines.len())
        {
            return Ok(());
        }

        let mut rows = Vec::with_capacity(updates.len());
        for (screen_row, line) in updates {
            let repaint_plan_row = *screen_row < self.animation_plan_rows
                && plan_row_requires_full_repaint(&self.previous_lines[*screen_row], line);
            let mut current = CellFrame::new(width, 1);
            let hovered = Self::hover_columns(line, self.hovered_tool, self.hovered_pick.as_ref());
            paint_line_into_frame(
                &mut current,
                0,
                line,
                None,
                hovered,
                self.side_panel.map(|layout| layout.main_width),
            );
            // A single-row repaint would otherwise blank the panel's cells on
            // that row, so redraw its slice of the panel alongside the line.
            if let Some(layout) = self.side_panel {
                paint_side_panel_row_into_frame(
                    &mut current,
                    layout,
                    0,
                    *screen_row,
                    self.previous_lines.len(),
                    &self.side_panel_content,
                    None,
                    &self.side_panel_footer,
                    self.hovered_pick.as_ref(),
                );
            }
            let start = screen_row * width;
            let previous = CellFrame {
                width,
                height: 1,
                cells: painted.cells[start..start + width].to_vec(),
            };
            rows.push((*screen_row, previous, current, repaint_plan_row));
        }

        queue!(self.out, Print("\x1b[?2026h"))?;
        let restore_cursor = self.cursor_shown;
        if restore_cursor {
            queue!(self.out, Hide)?;
        }
        let mut result = Ok(());
        for (screen_row, previous, current, repaint_plan_row) in &rows {
            // A changed plan step can shorten or restyle wide Korean text. Clear
            // just that row once; spinner frames keep the inexpensive diff path.
            let previous = (!*repaint_plan_row).then_some(previous);
            if let Err(error) = emit_frame_diff_at(&mut self.out, previous, current, *screen_row) {
                result = Err(error);
                break;
            }
        }
        if result.is_ok()
            && let Some(painted) = self.painted_frame.as_mut()
        {
            for (screen_row, _, current, _) in rows {
                let start = screen_row * width;
                painted.cells[start..start + width].clone_from_slice(&current.cells);
            }
        }
        queue!(
            self.out,
            MoveTo(
                self.cursor_col
                    .min(width.saturating_sub(2))
                    .min(u16::MAX as usize) as u16,
                self.cursor_line.min(u16::MAX as usize) as u16
            )
        )?;
        if restore_cursor {
            queue!(self.out, Show)?;
        }
        queue!(self.out, Print("\x1b[?2026l"))?;
        result?;
        self.painted_hovered_tool = self.hovered_tool;
        self.painted_hovered_pick = self.hovered_pick.clone();
        self.out.flush()?;
        Ok(())
    }

    /// One screen, bottom-anchored: the live frame takes the last rows and the
    /// transcript fills what is left above it. The composer is placed by row
    /// index rather than by trailing the last thing printed, which is the whole
    /// reason scrolling cannot carry it off screen.
    fn render_fullscreen(
        &mut self,
        committed: &[Block],
        mut frame: Frame,
        width: u16,
        total_width: u16,
        height: u16,
        plan_summary: Option<&PlanSummary>,
        activity_phase: f32,
        plan_active: bool,
        plan_shimmer_phase: Option<f32>,
        plan_effort: Option<&str>,
        waiting_for_response: bool,
        side_panel_prompts_expanded: bool,
        side_panel_integrations: &[ProviderIntegrationView],
        stream_fade_tail: usize,
    ) -> Result<()> {
        let composer_navigation_layout = frame.composer_layout.clone();
        let rows = height as usize;
        // The docked panel is where the plan lives while it is open, so the
        // transcript keeps its own full height and draws no card of its own.
        let plan_in_panel = self.side_panel.is_some();
        let plan_lines = plan_summary
            .filter(|_| !plan_in_panel)
            .map(|summary| {
                fixed_plan_summary_lines(
                    summary,
                    width,
                    activity_phase,
                    plan_active,
                    plan_shimmer_phase,
                    plan_effort,
                )
            })
            .unwrap_or_default();
        let plan_rows = plan_lines.len().min(rows.saturating_sub(1));
        let content_rows = rows.saturating_sub(plan_rows).max(1);
        if !committed.is_empty() {
            self.history_view_rows_anchor = None;
            self.history_view_start_anchor = None;
        }
        let old_view_rows = split_rows(content_rows, frame.lines.len(), self.wrapped.len()).0;
        self.commit_fullscreen_blocks(committed, width, old_view_rows);
        if self.scroll_back == 0 && self.wrapped.last() == Some(&PaintLine::blank()) {
            frame.absorb_leading_spacer();
        }
        let panel_content = self
            .side_panel
            .map(|layout| {
                let mut lines = plan_summary
                    .map(|summary| {
                        side_panel_plan_lines(
                            summary,
                            layout.content_width(),
                            activity_phase,
                            plan_active,
                        )
                    })
                    .unwrap_or_default();
                lines.extend(side_panel_prompt_lines(
                    &self.history,
                    layout.content_width(),
                    side_panel_prompts_expanded,
                ));
                if SIDE_PANEL_INTEGRATIONS_CONNECTED {
                    let content_capacity = rows.saturating_sub(self.side_panel_footer.len() + 2);
                    let remaining = content_capacity.saturating_sub(lines.len());
                    lines.extend(side_panel_integration_lines(
                        side_panel_integrations,
                        layout.content_width(),
                        remaining,
                    ));
                }
                lines
            })
            .unwrap_or_default();
        // A panel selection points at rows of this content, so content changes
        // leave the highlight describing something else.
        if self.selection_in_panel && panel_content.len() != self.side_panel_content.len() {
            self.selection.clear();
        }
        self.side_panel_content = panel_content;
        let provisional_view_rows = split_rows_with_transcript_anchor(
            content_rows,
            frame.lines.len(),
            self.wrapped.len(),
            self.history_view_rows_anchor,
        )
        .0;
        self.scroll_back = self
            .scroll_back
            .min(self.wrapped.len().saturating_sub(provisional_view_rows));
        let (view_rows, live_rows) = split_rows_with_transcript_anchor(
            content_rows,
            frame.lines.len(),
            self.wrapped.len(),
            self.history_view_rows_anchor,
        );
        self.last_transcript_rows = view_rows;
        // The live blocks run from the top of the frame down to the dock, and the
        // padding below goes in at the dock, so this row survives `fit_frame`.
        let stream_fade = (stream_fade_tail > 0 && frame.dock_index > 0).then(|| StreamFade {
            last_row: plan_rows + view_rows + frame.dock_index - 1,
            tail: stream_fade_tail,
        });
        // Padding the live frame is what puts the composer on the bottom row
        // *without* dragging the welcome card and the live blocks down with it:
        // `fit_frame` inserts the blanks at the dock, above the composer.
        fit_frame(&mut frame, live_rows);
        let max_back = self.wrapped.len() - view_rows;
        self.scroll_back = self.history_view_start_anchor.take().map_or_else(
            || self.scroll_back.min(max_back),
            |start| scroll_back_for_transcript_start(self.wrapped.len(), view_rows, start),
        );
        let start = max_back - self.scroll_back;
        let start = if plan_summary.is_some() && !plan_in_panel {
            transcript_start_below_plan(&self.wrapped, start)
        } else {
            start
        };
        let animation_activity_row = frame
            .activity_index
            .map(|index| plan_rows + view_rows + index);
        // The prompt rows follow the composer's top rule, and the live frame is
        // painted below the transcript window, so this is where they land.
        let composer_selection =
            frame
                .composer_index
                .zip(frame.composer_layout.take())
                .map(|(index, layout)| ComposerSelection {
                    first_row: plan_rows + view_rows + index + 1,
                    layout,
                });
        let (mut screen, cursor_line) = compose_screen(
            &self.wrapped,
            frame.lines,
            view_rows,
            start,
            frame.cursor_line,
        );
        screen.splice(0..0, plan_lines);
        let response_bullet_row = waiting_for_response
            .then(|| {
                visible_response_bullet_row(&self.wrapped, start..start + view_rows, plan_rows)
            })
            .flatten();
        let cursor_line = cursor_line + plan_rows;
        let scroll_to_bottom_overlay = self.scroll_to_bottom_control(width).and_then(|control| {
            let row = plan_rows + scroll_to_bottom_overlay_row(view_rows, frame.composer_index)?;
            let line = screen.get_mut(row)?;
            let start = UnicodeWidthStr::width(control.prefix.as_str());
            let end = start + UnicodeWidthStr::width(control.text.as_str());
            match line.pick.as_mut() {
                Some(picks) => picks.0.insert(0, (start, end, Pick::ScrollToBottom)),
                None => line.pick = Some(PickRegions::span(start, end, Pick::ScrollToBottom)),
            }
            Some((row, control))
        });
        self.last_transcript_start = start;
        self.last_transcript_screen_start = plan_rows;
        self.reconcile_selection(&screen, plan_rows);
        let full_repaint_rows = plan_rows_requiring_full_repaint(
            &self.previous_lines,
            self.animation_plan_rows,
            &screen,
            plan_rows,
        );
        let plan_geometry_changed = self.animation_plan_rows != plan_rows;
        self.paint_screen(
            &screen,
            cursor_line,
            frame.cursor_col,
            frame.show_cursor,
            total_width,
            scroll_to_bottom_overlay
                .as_ref()
                .map(|(row, control)| (*row, control)),
            &full_repaint_rows,
            plan_geometry_changed,
            stream_fade,
        )?;
        self.previous_lines = screen;
        self.cursor_line = cursor_line;
        self.cursor_col = frame.cursor_col;
        self.composer_navigation_layout = composer_navigation_layout;
        self.composer_selection = composer_selection;
        self.animation_activity_row = animation_activity_row;
        self.animation_response_bullet_row = response_bullet_row;
        self.animation_plan_rows = plan_rows;
        self.last_width = width;
        self.last_total_width = total_width;
        self.last_height = height;
        self.out.flush()?;
        Ok(())
    }

    fn commit_fullscreen_blocks(&mut self, committed: &[Block], width: u16, view_rows: usize) {
        if self.wrapped_width != width {
            let before = self.wrapped.len();
            for block in committed.iter().cloned() {
                merge_history_block(&mut self.history, block);
            }
            self.rewrap(width);
            if self.scroll_back > 0 {
                let row_delta = self.wrapped.len() as isize - before as isize;
                self.scroll_back = self.scroll_back.saturating_add_signed(row_delta);
            }
            return;
        }

        for block in committed {
            let before = self.wrapped.len();
            let replacement = self
                .history
                .iter()
                .position(|existing| existing.id() == block.id())
                .map(|index| {
                    let changed_start = self.history[..index]
                        .iter()
                        .flat_map(|existing| self.history_block_lines(existing, width))
                        .count();
                    let changed_end =
                        changed_start + self.history_block_lines(&self.history[index], width).len();
                    changed_start..changed_end
                });
            merge_history_block(&mut self.history, block.clone());

            if let Some(changed) = replacement {
                let viewport_start = before
                    .saturating_sub(view_rows)
                    .saturating_sub(self.scroll_back);
                let viewport_end = (viewport_start + view_rows).min(before);
                let relation = viewport_relation(changed, viewport_start..viewport_end);
                self.rewrap(width);
                // Before: rewrap moves the viewport start with the content.
                // Overlap: keep downstream visible content pinned. Only a
                // replacement wholly after the viewport changes its distance
                // from the bottom.
                if self.scroll_back > 0 && relation == ViewportRelation::After {
                    let row_delta = self.wrapped.len() as isize - before as isize;
                    self.scroll_back = self.scroll_back.saturating_add_signed(row_delta);
                }
            } else if matches!(block.kind, BlockKind::ProgressGroup)
                && self.fold_progress_groups
                && self.prompt_for_progress_group(block.id()).is_some()
            {
                // The new control lives in the already-wrapped prompt, so the
                // prompt and the hidden group must be rebuilt together.
                self.rewrap(width);
                if self.scroll_back > 0 {
                    let row_delta = self.wrapped.len() as isize - before as isize;
                    self.scroll_back = self.scroll_back.saturating_add_signed(row_delta);
                }
            } else {
                let lines = self.history_block_lines(block, width);
                if matches!(block.kind, BlockKind::ProgressGroup) && self.fold_progress_groups {
                    let start = self.wrapped.len();
                    let content_end = start
                        + lines
                            .len()
                            .saturating_sub(usize::from(lines.last() == Some(&PaintLine::blank())));
                    self.progress_group_rows.push(start..content_end);
                }
                self.wrapped.extend(lines);
                if self.scroll_back > 0 {
                    let row_delta = self.wrapped.len() as isize - before as isize;
                    self.scroll_back = self.scroll_back.saturating_add_signed(row_delta);
                }
            }
        }
    }

    fn rewrap(&mut self, width: u16) {
        let mut wrapped = Vec::new();
        let mut progress_group_rows = Vec::new();
        for block in visible_transcript_blocks(
            &self.history,
            self.shell_display_mode,
            self.diff_display_mode,
        ) {
            let lines = self.history_block_lines(block, width);
            if matches!(block.kind, BlockKind::ProgressGroup) && self.fold_progress_groups {
                let start = wrapped.len();
                let content_end = start
                    + lines
                        .len()
                        .saturating_sub(usize::from(lines.last() == Some(&PaintLine::blank())));
                progress_group_rows.push(start..content_end);
            }
            wrapped.extend(lines);
        }
        self.wrapped = wrapped;
        self.progress_group_rows = progress_group_rows;
        self.wrapped_width = width;
    }

    /// Repaints only the rows whose content changed. On the alternate screen
    /// nothing scrolls under us, so a row index is a stable address and the diff
    /// is a plain index-by-index comparison — this is what keeps typing from
    /// flickering the whole screen.
    fn paint_screen(
        &mut self,
        lines: &[PaintLine],
        cursor_line: usize,
        cursor_col: usize,
        show_cursor: bool,
        total_width: u16,
        scroll_to_bottom_overlay: Option<(usize, &PaintLine)>,
        full_repaint_rows: &[usize],
        repaint_full_frame: bool,
        stream_fade: Option<StreamFade>,
    ) -> Result<()> {
        let selection = self.selection.range().filter(|range| {
            if self.selection_in_panel {
                selection_is_worth_painting(*range, &self.side_panel_content)
            } else if self.selection_in_transcript {
                selection_is_worth_painting(*range, &self.wrapped)
            } else {
                selection_is_worth_painting(*range, lines)
            }
        });
        let transcript_selection = selection.filter(|_| !self.selection_in_panel);
        let panel_selection = selection.filter(|_| self.selection_in_panel);
        let mut frame = CellFrame::new(usize::from(total_width), lines.len());
        for (row, line) in lines.iter().enumerate() {
            let hovered = Self::hover_columns(line, self.hovered_tool, self.hovered_pick.as_ref());
            let selected_columns = transcript_selection.and_then(|range| {
                let selection_row = if self.selection_in_transcript {
                    self.transcript_row_at_screen(row)?
                } else {
                    row
                };
                selection_columns_for_line(line, range, selection_row)
            });
            paint_line_into_frame(
                &mut frame,
                row,
                line,
                selected_columns,
                hovered,
                self.side_panel.map(|layout| layout.main_width),
            );
        }
        if let Some((row, control)) = scroll_to_bottom_overlay {
            let hovered = self.hovered_pick.as_ref() == Some(&Pick::ScrollToBottom);
            paint_scroll_to_bottom_into_frame(&mut frame, row, control, hovered);
        }
        if let Some(fade) = stream_fade {
            fade_stream_tail_into_frame(&mut frame, fade);
        }
        if let Some(layout) = self.side_panel {
            paint_side_panel_into_frame_with_footer(
                &mut frame,
                layout,
                lines.len(),
                &self.side_panel_content,
                panel_selection,
                &self.side_panel_footer,
                self.hovered_pick.as_ref(),
            );
        }
        emit_synchronized_frame_diff_with_full_rows(
            &mut self.out,
            self.painted_frame.as_ref(),
            &frame,
            full_repaint_rows,
            repaint_full_frame,
            Some((
                cursor_col
                    .min(usize::from(total_width).saturating_sub(2))
                    .min(u16::MAX as usize) as u16,
                cursor_line.min(u16::MAX as usize) as u16,
                show_cursor,
            )),
            self.cursor_shown,
        )?;
        self.painted_frame = Some(frame);
        self.painted_selection = selection;
        self.painted_hovered_tool = self.hovered_tool;
        self.painted_hovered_pick = self.hovered_pick.clone();
        self.cursor_shown = show_cursor;
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.mode == RenderMode::Fullscreen {
            // Leaving the alternate screen discards the frame wholesale, so
            // there is nothing to erase and no room to leave behind.
            queue!(self.out, Show, ResetColor)?;
            self.out.flush()?;
            return Ok(());
        }
        self.erase_live()?;
        queue!(self.out, Show, ResetColor, Print("\r\n"))?;
        self.out.flush()?;
        Ok(())
    }

    fn erase_live(&mut self) -> Result<()> {
        if self.previous_lines.is_empty() || self.mode == RenderMode::Fullscreen {
            return Ok(());
        }

        let available_up = cursor_position()
            .map(|(_, row)| row as usize)
            .unwrap_or(self.cursor_line);
        let move_up = self.cursor_line.min(available_up);
        queue!(self.out, MoveToColumn(0))?;
        if move_up > 0 {
            queue!(self.out, MoveUp(move_up.min(u16::MAX as usize) as u16))?;
        }
        queue!(self.out, Clear(ClearType::FromCursorDown))?;
        self.previous_lines.clear();
        self.cursor_line = 0;
        self.cursor_col = 0;
        Ok(())
    }

    fn print_permanent(&mut self, block: &Block, lines: &[PaintLine]) -> Result<()> {
        let tagged = copy_metadata_applies(block.kind);
        for line in lines {
            if tagged {
                let marker_skip = 0;
                let join_next = usize::from(copy_joins_next(line));
                let prefix_width = UnicodeWidthStr::width(line.prefix.as_str());
                queue!(
                    self.out,
                    Print(format!(
                        "\x1b]777;devez-copy-v1;{marker_skip};{join_next};{prefix_width}\x07"
                    ))
                )?;
            }
            print_line(&mut self.out, line)?;
            queue!(self.out, Print("\r\n"))?;
        }
        self.out.flush()?;
        Ok(())
    }

    fn print_frame_full(&mut self, frame: &Frame) -> Result<()> {
        queue!(self.out, Hide)?;
        for (index, line) in frame.lines.iter().enumerate() {
            print_line(&mut self.out, line)?;
            if index + 1 < frame.lines.len() {
                queue!(self.out, Print("\r\n"))?;
            }
        }

        let bottom_distance = (frame.lines.len() - 1 - frame.cursor_line) as u16;
        if bottom_distance > 0 {
            queue!(self.out, MoveUp(bottom_distance))?;
        }
        queue!(
            self.out,
            MoveToColumn(frame.cursor_col.min(u16::MAX as usize) as u16)
        )?;
        if frame.show_cursor {
            queue!(self.out, Show)?;
        }
        Ok(())
    }

    fn patch_frame(&mut self, frame: &Frame) -> Result<()> {
        queue!(self.out, Hide, Print("\x1b[?2026h"))?;
        let result = self.patch_frame_inner(frame);
        let end = queue!(self.out, Print("\x1b[?2026l"));
        match (result, end) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error.into()),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    fn patch_frame_inner(&mut self, frame: &Frame) -> Result<()> {
        let old_len = self.previous_lines.len();
        let new_len = frame.lines.len();
        if old_len == 0 || new_len == 0 {
            self.erase_live()?;
            return self.print_frame_full(frame);
        }

        let mut current_row = self.cursor_line.min(old_len - 1);

        if new_len > old_len {
            move_to_row(&mut self.out, &mut current_row, old_len - 1)?;
            for _ in old_len..new_len {
                queue!(self.out, Print("\r\n"))?;
                current_row += 1;
            }
        }

        for row in 0..new_len {
            let changed = self.previous_lines.get(row) != frame.lines.get(row);
            if changed {
                move_to_row(&mut self.out, &mut current_row, row)?;
                queue!(self.out, MoveToColumn(0))?;
                print_line(&mut self.out, &frame.lines[row])?;
                queue!(self.out, Clear(ClearType::UntilNewLine))?;
            }
        }

        if old_len > new_len {
            for row in new_len..old_len {
                move_to_row(&mut self.out, &mut current_row, row)?;
                queue!(self.out, Clear(ClearType::CurrentLine))?;
            }
        }

        move_to_row(
            &mut self.out,
            &mut current_row,
            frame.cursor_line.min(new_len - 1),
        )?;
        queue!(
            self.out,
            MoveToColumn(frame.cursor_col.min(u16::MAX as usize) as u16)
        )?;
        if frame.show_cursor {
            queue!(self.out, Show)?;
        }
        Ok(())
    }
}

fn plan_row_requires_full_repaint(previous: &PaintLine, current: &PaintLine) -> bool {
    previous.prefix_tone != current.prefix_tone
        || previous.text != current.text
        || previous.tone != current.tone
        || previous.bold != current.bold
        || !previous
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .eq(current.tail.iter().map(|span| span.text.as_str()))
}

/// A plan state change first reaches the normal render path, before the next
/// animation tick can compare its rows. Mark changed step rows here so they get
/// one safe full repaint in that first synchronized frame. Spinner-only prefix
/// changes keep using the inexpensive cell diff.
fn plan_rows_requiring_full_repaint(
    previous: &[PaintLine],
    previous_plan_rows: usize,
    current: &[PaintLine],
    current_plan_rows: usize,
) -> Vec<usize> {
    if previous_plan_rows != current_plan_rows {
        return (0..current_plan_rows).collect();
    }
    (0..current_plan_rows)
        .filter(|&row| {
            previous
                .get(row)
                .zip(current.get(row))
                .is_some_and(|(before, after)| plan_row_requires_full_repaint(before, after))
        })
        .collect()
}

/// Splits the screen between the transcript and the live frame, as
/// `(transcript rows, live rows)`. The live frame gets everything the transcript
/// cannot fill, and is padded to that height rather than floated, so the composer
/// reaches the bottom row while the welcome card stays put at the top.
fn split_rows(rows: usize, live_natural: usize, transcript_len: usize) -> (usize, usize) {
    let rows = rows.max(1);
    // A live frame taller than the screen leaves the transcript nothing, and
    // `fit_frame` trims the frame's oldest rows to make it fit.
    let view_rows = transcript_len.min(rows.saturating_sub(live_natural));
    (view_rows, rows - view_rows)
}

/// A History disclosure changes only which transcript rows are visible. Keep
/// the transcript's previous allocation until new output, scrolling, or a
/// geometry change gives the viewport a new reason to move.
fn split_rows_with_transcript_anchor(
    rows: usize,
    live_natural: usize,
    transcript_len: usize,
    anchored_view_rows: Option<usize>,
) -> (usize, usize) {
    let rows = rows.max(1);
    let Some(anchored_view_rows) = anchored_view_rows else {
        return split_rows(rows, live_natural, transcript_len);
    };
    let capacity = rows.saturating_sub(live_natural);
    let view_rows = anchored_view_rows.min(transcript_len).min(capacity);
    (view_rows, rows - view_rows)
}

fn scroll_back_for_transcript_start(
    transcript_len: usize,
    view_rows: usize,
    transcript_start: usize,
) -> usize {
    let max_back = transcript_len.saturating_sub(view_rows);
    max_back.saturating_sub(transcript_start.min(max_back))
}

#[allow(dead_code)]
fn clear_main_row(out: &mut impl Write, row: usize, width: usize) -> Result<()> {
    queue!(
        out,
        MoveTo(0, row.min(u16::MAX as usize) as u16),
        ResetColor,
        Print(" ".repeat(width)),
        MoveTo(0, row.min(u16::MAX as usize) as u16),
    )?;
    Ok(())
}

fn cell_style(tone: Tone, bold: bool, background: Option<Rgb>, selected: bool) -> CellStyle {
    if selected {
        return CellStyle {
            foreground: Some(
                tone_rgb(tone).map_or_else(theme::selection_fg, theme::selection_text),
            ),
            background: Some(theme::selection_bg()),
            bold,
            italic: false,
            underlined: false,
            crossed_out: false,
        };
    }
    CellStyle {
        foreground: tone_rgb(tone),
        background,
        bold,
        italic: tone == Tone::Thinking,
        underlined: tone == Tone::MarkdownLink,
        crossed_out: tone == Tone::PlanDone,
    }
}

fn range_overlaps(columns: Option<&Range<usize>>, start: usize, end: usize) -> bool {
    columns.is_some_and(|columns| start < columns.end && columns.start < end)
}

fn paint_text_into_frame(
    frame: &mut CellFrame,
    row: usize,
    text: &str,
    column: &mut usize,
    tone: Tone,
    bold: bool,
    background: Option<Rgb>,
    selected_columns: Option<&Range<usize>>,
    hovered_columns: Option<&Range<usize>>,
) {
    for unit in display_units(text) {
        let width = terminal_unit_width(unit);
        let end = column.saturating_add(width);
        let selected = range_overlaps(selected_columns, *column, end);
        let hovered = !selected && range_overlaps(hovered_columns, *column, end);
        let style = cell_style(
            tone,
            bold,
            if hovered {
                Some(theme::palette().hover_bg)
            } else {
                background
            },
            selected,
        );
        frame.write(*column, row, unit, style);
        *column = end;
    }
}

fn paint_line_into_frame(
    frame: &mut CellFrame,
    row: usize,
    line: &PaintLine,
    selected_columns: Option<Range<usize>>,
    hovered_columns: Option<Range<usize>>,
    background_width: Option<usize>,
) {
    let background = row_background(line.tone);
    let bubble_background = bubble_background(line);
    if let Some(background) = background {
        // 사이드패널이 열려 있으면 본문 폭이 화면 폭보다 좁다. 화면 끝까지
        // 칠하는 줄도 본문 폭 안에서 끝나야 배경이 패널 쪽으로 번지지 않는다.
        let trailing_right = background_width.unwrap_or(frame.width).saturating_sub(1);
        let (start, right) = if matches!(line.tone, Tone::UserPrompt | Tone::UserPromptPadding) {
            let prefix_width = UnicodeWidthStr::width(line.prefix.as_str());
            let start = if CHAT_LAYOUT.load(Ordering::Relaxed) {
                let marker_width = usize::from(line.prefix.ends_with("› "))
                    * (CHAT_BUBBLE_PADDING + CHAT_BUBBLE_RIGHT_GAP + 2);
                prefix_width.saturating_sub(marker_width + 1)
            } else {
                prefix_width
            };
            (start, trailing_right)
        } else if line.tone == Tone::ModelChange {
            // Setting-change cards share the user's terminal-safe trailing cell.
            // Fast, Model, and Effort notices therefore end on the same column.
            (0, trailing_right)
        } else {
            (0, background_width.unwrap_or(frame.width))
        };
        frame.fill(
            start,
            row,
            right,
            row + 1,
            CellStyle {
                background: Some(background),
                ..CellStyle::plain()
            },
        );
    }
    let history_columns = line.pick.as_ref().and_then(|regions| {
        regions
            .0
            .iter()
            .find_map(|(start, end, pick)| matches!(pick, Pick::History(_)).then_some(*start..*end))
    });
    let history_hovered = hovered_columns.as_ref().is_some_and(|hovered| {
        history_columns
            .as_ref()
            .is_some_and(|columns| range_overlaps(Some(hovered), columns.start, columns.end))
    });
    let history_background = history_columns.as_ref().map(|_| {
        if history_hovered {
            scroll_to_bottom_background(true)
        } else {
            theme::palette().user_prompt_bg
        }
    });
    if let (Some(columns), Some(background)) = (history_columns.as_ref(), history_background) {
        frame.fill(
            columns.start,
            row,
            columns.end.min(frame.width),
            row + 1,
            CellStyle {
                background: Some(background),
                ..CellStyle::plain()
            },
        );
    }
    let text_hovered_columns = (!history_hovered)
        .then_some(hovered_columns.as_ref())
        .flatten();
    let mut column = 0;
    let prefix_width = UnicodeWidthStr::width(line.prefix.as_str());
    let history_prefix_background = history_columns
        .as_ref()
        .filter(|columns| columns.start < prefix_width)
        .and(history_background);
    let prefix_background = history_prefix_background.or_else(|| {
        word_background(line.prefix_tone)
            .or(bubble_background)
            .or(background)
    });
    paint_text_into_frame(
        frame,
        row,
        &line.prefix,
        &mut column,
        line.prefix_tone,
        false,
        prefix_background,
        selected_columns.as_ref(),
        text_hovered_columns,
    );
    paint_text_into_frame(
        frame,
        row,
        &line.text,
        &mut column,
        line.tone,
        line.bold,
        history_background.or_else(|| {
            word_background(line.tone)
                .or(bubble_background)
                .or(background)
        }),
        selected_columns.as_ref(),
        text_hovered_columns,
    );
    for span in &line.tail {
        if span.tone == Tone::CopyJoin {
            continue;
        }
        paint_text_into_frame(
            frame,
            row,
            &span.text,
            &mut column,
            span.tone,
            span.bold,
            history_background.or_else(|| {
                word_background(span.tone)
                    .or(bubble_background)
                    .or(background)
            }),
            selected_columns.as_ref(),
            text_hovered_columns,
        );
    }
}

fn set_cell_style(out: &mut impl Write, style: CellStyle) -> Result<()> {
    queue!(
        out,
        SetAttribute(Attribute::Reset),
        ResetColor,
        SetBackgroundColor(style.background.map_or(Color::Reset, rgb_color)),
        SetForegroundColor(style.foreground.map_or(Color::Reset, rgb_color))
    )?;
    if style.bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    if style.italic {
        queue!(out, SetAttribute(Attribute::Italic))?;
    }
    if style.underlined {
        queue!(out, SetAttribute(Attribute::Underlined))?;
    }
    if style.crossed_out {
        queue!(out, SetAttribute(Attribute::CrossedOut))?;
    }
    Ok(())
}

fn emit_frame_diff(
    out: &mut impl Write,
    previous: Option<&CellFrame>,
    current: &CellFrame,
) -> Result<()> {
    emit_frame_diff_at(out, previous, current, 0)
}

fn emit_frame_diff_at(
    out: &mut impl Write,
    previous: Option<&CellFrame>,
    current: &CellFrame,
    row_offset: usize,
) -> Result<()> {
    let previous =
        previous.filter(|frame| frame.width == current.width && frame.height == current.height);
    for row in 0..current.height {
        let screen_row = row.saturating_add(row_offset);
        let wide_damage = previous.and_then(|previous| wide_damage_range(previous, current, row));
        if let Some(columns) = wide_damage.clone() {
            emit_frame_columns(out, current, row, screen_row, columns)?;
        }
        let mut column = 0;
        while column < current.width {
            if wide_damage
                .as_ref()
                .is_some_and(|columns| columns.contains(&column))
            {
                column += 1;
                continue;
            }
            let cell = current.cell(column, row);
            let changed = previous.is_none_or(|previous| cell != previous.cell(column, row));
            if !changed || cell.continuation {
                column += 1;
                continue;
            }
            if column + 1 == current.width {
                // The terminal erase fills its final visual cell with this
                // background without printing into the autowrap column.
                queue!(
                    out,
                    MoveTo(
                        column.min(u16::MAX as usize) as u16,
                        screen_row.min(u16::MAX as usize) as u16
                    )
                )?;
                set_cell_style(out, cell.style)?;
                queue!(out, Clear(ClearType::UntilNewLine))?;
                column += 1;
                continue;
            }

            let start = column;
            let style = cell.style;
            let mut text = String::new();
            while column + 1 < current.width {
                if wide_damage
                    .as_ref()
                    .is_some_and(|columns| columns.contains(&column))
                {
                    break;
                }
                let cell = current.cell(column, row);
                let changed = previous.is_none_or(|previous| cell != previous.cell(column, row));
                if !changed || (!cell.continuation && cell.style != style) {
                    break;
                }
                if !cell.continuation {
                    text.push_str(&cell.glyph);
                }
                column += 1;
            }
            queue!(
                out,
                MoveTo(
                    start.min(u16::MAX as usize) as u16,
                    screen_row.min(u16::MAX as usize) as u16
                )
            )?;
            set_cell_style(out, style)?;
            queue!(out, Print(text))?;
        }
    }
    queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    Ok(())
}

/// Returns the smallest safe repaint range around changed double-width glyphs.
/// The range starts before any old/current continuation cell and ends after it,
/// so a terminal is never asked to paint into the trailing half of a glyph.
/// Keeping this local avoids clearing and flashing the entire composer row for
/// every Korean character typed.
fn wide_damage_range(
    previous: &CellFrame,
    current: &CellFrame,
    row: usize,
) -> Option<Range<usize>> {
    let mut changed = (0..current.width).filter(|&column| {
        let before = previous.cell(column, row);
        let after = current.cell(column, row);
        before != after
            && (before.continuation
                || after.continuation
                || UnicodeWidthStr::width(before.glyph.as_str()) > 1
                || UnicodeWidthStr::width(after.glyph.as_str()) > 1)
    });
    let mut start = changed.next()?;
    let mut end = changed.next_back().unwrap_or(start) + 1;

    while start > 0
        && (previous.cell(start, row).continuation || current.cell(start, row).continuation)
    {
        start -= 1;
    }
    while end < current.width
        && (previous.cell(end, row).continuation || current.cell(end, row).continuation)
    {
        end += 1;
    }
    Some(start..end)
}

fn emit_frame_columns(
    out: &mut impl Write,
    frame: &CellFrame,
    row: usize,
    screen_row: usize,
    columns: Range<usize>,
) -> Result<()> {
    let mut column = columns.start.min(frame.width);
    let end = columns.end.min(frame.width);
    while column < end {
        let cell = frame.cell(column, row);
        if cell.continuation {
            column += 1;
            continue;
        }
        if column + 1 == frame.width {
            queue!(
                out,
                MoveTo(
                    column.min(u16::MAX as usize) as u16,
                    screen_row.min(u16::MAX as usize) as u16
                )
            )?;
            set_cell_style(out, cell.style)?;
            queue!(out, Clear(ClearType::UntilNewLine))?;
            column += 1;
            continue;
        }
        let style = cell.style;
        let start = column;
        let mut text = String::new();
        while column < end && column + 1 < frame.width {
            let cell = frame.cell(column, row);
            if !cell.continuation && cell.style != style {
                break;
            }
            if cell.continuation {
                column += 1;
            } else {
                text.push_str(&cell.glyph);
                column += terminal_unit_width(&cell.glyph).max(1);
            }
        }
        queue!(
            out,
            MoveTo(
                start.min(u16::MAX as usize) as u16,
                screen_row.min(u16::MAX as usize) as u16
            )
        )?;
        set_cell_style(out, style)?;
        queue!(out, Print(text))?;
    }
    Ok(())
}

/// A semantic plan change can alter the fixed panel height, which also moves
/// transcript and composer rows. Repaint the complete frame once so no old row
/// remains at its former terminal position. Spinner-only frames still use the
/// ordinary incremental diff.
fn emit_synchronized_frame_diff_with_full_rows(
    out: &mut impl Write,
    previous: Option<&CellFrame>,
    current: &CellFrame,
    full_rows: &[usize],
    repaint_full_frame: bool,
    cursor: Option<(u16, u16, bool)>,
    cursor_shown: bool,
) -> Result<()> {
    queue!(out, Print("\x1b[?2026h"))?;
    // Hide only for a semantic plan repaint. Spinner frames never reach this
    // path, so the cursor is restored once rather than toggled every 80ms.
    let repainting_plan = repaint_full_frame || !full_rows.is_empty();
    let cursor_moves_outside_composer = cursor.is_some_and(|(_, row, show)| {
        show && cursor_shown && frame_changed_outside_row(previous, current, usize::from(row))
    });
    let hide_cursor = cursor_shown
        && cursor
            .is_some_and(|(_, _, show)| !show || repainting_plan || cursor_moves_outside_composer);
    if hide_cursor {
        queue!(out, Hide)?;
    }
    let mut result = Ok(());
    if repaint_full_frame {
        result = emit_frame_diff(out, None, current);
    } else {
        let mut diff_previous = previous.cloned();
        for &row in full_rows {
            if row >= current.height {
                continue;
            }
            let row_frame = CellFrame {
                width: current.width,
                height: 1,
                cells: current.cells[row * current.width..(row + 1) * current.width].to_vec(),
            };
            if let Err(error) = emit_frame_diff_at(out, None, &row_frame, row) {
                result = Err(error);
                break;
            }
            if let Some(previous) = diff_previous.as_mut().filter(|previous| {
                previous.width == current.width && previous.height == current.height
            }) {
                previous.cells[row * current.width..(row + 1) * current.width]
                    .clone_from_slice(&row_frame.cells);
            }
        }
        if result.is_ok() {
            result = emit_frame_diff(out, diff_previous.as_ref().or(previous), current);
        }
    }
    if let Some((column, row, show)) = cursor {
        queue!(out, MoveTo(column, row))?;
        if show && (!cursor_shown || hide_cursor) {
            queue!(out, Show)?;
        }
    }
    let end = queue!(out, Print("\x1b[?2026l"));
    match (result, end) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn frame_changed_outside_row(
    previous: Option<&CellFrame>,
    current: &CellFrame,
    cursor_row: usize,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.width != current.width || previous.height != current.height {
        return true;
    }
    (0..current.height).any(|row| {
        if row == cursor_row {
            return false;
        }
        let start = row * current.width;
        let end = start + current.width;
        previous.cells[start..end] != current.cells[start..end]
    })
}

/// Lays out one fullscreen frame: `view_rows` of transcript from `start`, then the
/// live frame. Reports the cursor's row in the result. Pure, so the anchoring can
/// be asserted without a terminal to paint into.
fn compose_screen(
    wrapped: &[PaintLine],
    live: Vec<PaintLine>,
    view_rows: usize,
    start: usize,
    live_cursor_line: usize,
) -> (Vec<PaintLine>, usize) {
    let start = start.min(wrapped.len());
    let end = (start + view_rows).min(wrapped.len());
    let mut screen = Vec::with_capacity(view_rows + live.len());
    screen.extend(wrapped[start..end].iter().cloned());
    // `split_rows` never asks for more transcript than there is, so this only
    // guards the invariant rather than laying anything out.
    screen.resize(view_rows, PaintLine::blank());
    let cursor_line = screen.len() + live_cursor_line;
    screen.extend(live);
    (screen, cursor_line)
}

const RESPONSE_BULLET_PREFIX: &str = "• ";

fn visible_response_bullet_row(
    wrapped: &[PaintLine],
    visible: Range<usize>,
    rows_before_transcript: usize,
) -> Option<usize> {
    let row = wrapped.iter().rposition(|line| {
        line.prefix == RESPONSE_BULLET_PREFIX && line.prefix_tone == Tone::FastOff
    })?;
    visible
        .contains(&row)
        .then(|| rows_before_transcript + row - visible.start)
}

/// The plan already owns one blank row below its bottom border. If a scrolled
/// transcript window lands on block-separator blanks, start at its next visible
/// row so those separators do not stack below the fixed plan.
fn transcript_start_below_plan(wrapped: &[PaintLine], start: usize) -> usize {
    let start = start.min(wrapped.len());
    start
        + wrapped[start..]
            .iter()
            .take_while(|line| **line == PaintLine::blank())
            .count()
}

/// The overlay floats two rows above the composer, covering only the button's
/// cells rather than claiming a transcript row of its own.
fn scroll_to_bottom_overlay_row(view_rows: usize, composer_index: Option<usize>) -> Option<usize> {
    composer_index.map(|index| view_rows + index.saturating_sub(3))
}

fn move_to_row(out: &mut impl Write, current_row: &mut usize, target_row: usize) -> Result<()> {
    match target_row.cmp(current_row) {
        std::cmp::Ordering::Greater => {
            queue!(
                out,
                MoveDown((target_row - *current_row).min(u16::MAX as usize) as u16)
            )?;
        }
        std::cmp::Ordering::Less => {
            queue!(
                out,
                MoveUp((*current_row - target_row).min(u16::MAX as usize) as u16)
            )?;
        }
        std::cmp::Ordering::Equal => {}
    }
    queue!(out, MoveToColumn(0))?;
    *current_row = target_row;
    Ok(())
}

struct Frame {
    lines: Vec<PaintLine>,
    cursor_line: usize,
    cursor_col: usize,
    show_cursor: bool,
    dock_index: usize,
    composer_index: Option<usize>,
    /// The prompt rows of the composer this frame carries, if it has one, so a
    /// drag over them can be mapped back to composer characters.
    composer_layout: Option<ComposerLayout>,
    activity_index: Option<usize>,
}

impl Frame {
    /// A committed transcript block already ends in its own separator. When the
    /// pinned frame starts with another spacer, remove that duplicate and shift
    /// every frame-local address with it so the completed answer stays put.
    fn absorb_leading_spacer(&mut self) -> bool {
        if self.dock_index != 0 || self.lines.first() != Some(&PaintLine::blank()) {
            return false;
        }
        self.lines.remove(0);
        self.cursor_line = self.cursor_line.saturating_sub(1);
        self.composer_index = self.composer_index.map(|index| index.saturating_sub(1));
        self.activity_index = self.activity_index.map(|index| index.saturating_sub(1));
        true
    }
}

struct StatusArea {
    fallback: String,
    line: Option<StatusLineView>,
    composer_notice: Option<String>,
    composer_mode: Option<ComposerMode>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tone {
    Plain,
    Muted,
    /// Muted *and* italic: reserved for reasoning summaries.
    Thinking,
    /// Muted *and* struck through: a plan step that is already done.
    PlanDone,
    Accent,
    User,
    /// A centred transcript control: default text on a compact button band.
    ScrollToBottom,
    /// Prompt-hosted History text, softened without becoming fully muted.
    History,
    #[allow(dead_code)]
    Success,
    Warning,
    Error,
    Code,
    EffortLow,
    EffortMedium,
    EffortHigh,
    EffortXHigh,
    EffortMax,
    EffortUltra,
    StatusText,
    StatusSeparator,
    UserPrompt,
    UserPromptPadding,
    AssistantBubble,
    AssistantBubbleHalf,
    Model56,
    ModelSol,
    ModelTerra,
    ModelLuna,
    ModelSpark,
    Model55,
    ModelHaiku,
    ModelSonnet,
    ModelOpus,
    ModelFable,
    StatusModel56,
    StatusModelSol,
    StatusModelTerra,
    StatusModelLuna,
    StatusModelSpark,
    StatusModel55,
    StatusModelHaiku,
    StatusModelSonnet,
    StatusModelOpus,
    StatusModelFable,
    StatusEffortLow,
    StatusEffortMedium,
    StatusEffortHigh,
    StatusEffortXHigh,
    StatusEffortMax,
    StatusEffortUltra,
    Border,
    SidePanelDivider,
    Branch,
    #[allow(dead_code)]
    LimitFiveHour,
    #[allow(dead_code)]
    LimitWeekly,
    FastOn,
    FastOff,
    VibeSuper,
    ClaudeAcceptEdits,
    ClaudePlan,
    ClaudeAuto,
    ClaudeBypass,
    ModelChange,
    SyntaxComment,
    SyntaxString,
    SyntaxKeyword,
    SyntaxNumber,
    SyntaxType,
    SyntaxFunction,
    SyntaxAttribute,
    SyntaxProperty,
    MarkdownHeading,
    MarkdownLink,
    InlineCode,
    DiffAdded,
    DiffRemoved,
    /// The words a `+`/`-` row actually changed against its counterpart. Same
    /// text colour as the row, a stronger tint underneath.
    DiffAddedWord,
    DiffRemovedWord,
    DiffHeader,
    /// One character of the shimmering `Working` label. The payload is how far
    /// the sweep's bright band has reached that character, `0` for untouched.
    Shimmer(Rgb, u8),
    /// One character of a plan border shimmer, blended from the normal border
    /// colour toward the current effort colour.
    PlanShimmer(Rgb, u8),
    /// The oldest row still visible while progress messages fold upward.
    ResponseTransition(Rgb, u8),
    CopyJoin,
}

/// What a left click on a painted row means. The overlay row index is the
/// position in `OverlayView::lines`, not an absolute item index: only the picker
/// that built those rows knows how they map back. The rest are the session's own
/// chrome — the badges on the composer rule and the readings on the status line,
/// each standing for the command or key that changes the same setting.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Pick {
    Row(usize),
    Effort(usize),
    RemoveQueuedPrompt(usize),
    /// A Markdown link in the transcript opens through the platform handler.
    OpenLink(String),
    /// The Vibe preset applies its response and transcript display settings.
    VibeMode,
    /// Legacy internal picks retained for command and regression-test routing.
    #[allow(dead_code)]
    ResponseLength,
    ShellDisplayMode,
    DiffDisplayMode,
    PlanSummary,
    PromptSection,
    McpSection(String),
    PluginSection(String),
    /// The `Fast: On`/`Fast: Off` badge: toggles the fast service tier.
    FastMode,
    /// Claude's permission mode badge: cycles the mode the way Shift+Tab does
    /// in the Claude Code CLI.
    ClaudePermissionMode,
    /// The status line's model name: opens `/model`.
    Model,
    /// The status line's effort reading: opens `/effort`.
    EffortSetting,
    /// A running-subagent row under the status line: opens its transcript panel.
    Subagent(usize),
    /// The fullscreen transcript control that returns to its newest row.
    ScrollToBottom,
    /// A completed response's progress disclosure, hosted by its user prompt.
    History(u64),
    /// A recent prompt row in the docked panel: jumps to that transcript block.
    Prompt(u64),
    /// The `✕` on a panel's top rule: closes what Esc closes.
    Close,
}

/// Columns a clickable span reaches past its own text, either side. A word is a
/// small target to hit with a pointer, and the highlight reads as a button rather
/// than as underlined text with the breathing room. Every separator this code
/// paints between two clickable spans is wider than twice this, so the regions
/// still cannot run into one another.
const PICK_BLEED: usize = 1;

/// The clickable column spans of one painted row.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PickRegions(Vec<(usize, usize, Pick)>);

impl PickRegions {
    fn span(start: usize, end: usize, pick: Pick) -> Self {
        Self(vec![(start, end, pick)])
    }

    fn at(&self, column: usize) -> Option<Pick> {
        self.0
            .iter()
            .find(|(start, end, _)| column >= *start && column < *end)
            .map(|(_, _, pick)| pick.clone())
    }

    /// Where a pick was painted on this row, if it was.
    fn columns_of(&self, pick: &Pick) -> Option<Range<usize>> {
        self.0
            .iter()
            .filter(|(_, _, candidate)| candidate == pick)
            .fold(None, |range: Option<Range<usize>>, (start, end, _)| {
                Some(match range {
                    Some(range) => range.start.min(*start)..range.end.max(*end),
                    None => *start..*end,
                })
            })
    }

    fn shifted(mut self, columns: usize) -> Self {
        for (start, end, _) in &mut self.0 {
            *start = start.saturating_add(columns);
            *end = end.saturating_add(columns);
        }
        self
    }
}

#[derive(Clone, PartialEq, Eq)]
struct PaintLine {
    prefix: String,
    prefix_tone: Tone,
    text: String,
    tone: Tone,
    bold: bool,
    tool_heading: Option<u64>,
    pick: Option<PickRegions>,
    tail: Vec<PaintSpan>,
}

#[derive(Clone, PartialEq, Eq)]
struct PaintSpan {
    text: String,
    tone: Tone,
    bold: bool,
}

impl PaintLine {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            prefix: String::new(),
            prefix_tone: Tone::Plain,
            text: text.into(),
            tone: Tone::Plain,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        }
    }

    fn blank() -> Self {
        Self::plain("")
    }

    fn user_prompt_padding(width: usize) -> Self {
        Self {
            prefix: String::new(),
            prefix_tone: Tone::Plain,
            text: " ".repeat(width),
            tone: Tone::UserPromptPadding,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        }
    }

    /// Makes single spans of an already-built row clickable. `picks` addresses
    /// spans in paint order — `0` is `text`, `1` onward is `tail` — so a caller
    /// marks the badge it just placed without having to know what the row's
    /// prefix or the spans ahead of it cost in columns.
    fn with_picks(mut self, picks: &[(usize, Pick)]) -> Self {
        let mut column = UnicodeWidthStr::width(self.prefix.as_str());
        let mut regions = Vec::new();
        for (index, text) in std::iter::once(self.text.as_str())
            .chain(self.tail.iter().map(|span| span.text.as_str()))
            .enumerate()
        {
            let width = UnicodeWidthStr::width(text);
            if width > 0
                && let Some((_, pick)) = picks.iter().find(|(at, _)| *at == index)
            {
                // The status model badge already owns a padded background,
                // so its hover stops at that badge instead of bleeding into
                // the adjacent separators.
                let bleed = if matches!(pick, Pick::Model) {
                    0
                } else {
                    PICK_BLEED
                };
                regions.push((
                    column.saturating_sub(bleed),
                    column + width + bleed,
                    pick.clone(),
                ));
            }
            column += width;
        }
        if !regions.is_empty() {
            self.pick = Some(PickRegions(regions));
        }
        self
    }
}

/// How many characters the bright band of the shimmer covers at once. Wide
/// enough that the sweep reads as a gliding highlight rather than a cursor
/// running along the word, and narrow enough that a word as short as `Working`
/// still has dim characters ahead of the band — otherwise the whole label just
/// pulses in place.
const SHIMMER_BAND: f32 = 3.0;
const PLAN_SHIMMER_BAND: f32 = SHIMMER_BAND * 2.5;
const PLAN_SHIMMER_LOOPS: f32 = 5.0;

fn waiting_response_bullet_tone(phase: f32) -> Tone {
    let wave = 0.5 - 0.5 * (phase.clamp(0.0, 1.0) * std::f32::consts::TAU).cos();
    let base = tone_rgb(Tone::FastOff).unwrap_or(theme::palette().muted);
    Tone::Shimmer(base, (wave * 255.0).round() as u8)
}

/// One span per character, each lit by how close it is to the band's centre, so
/// the label carries a soft gradient instead of a hard block. `phase` runs
/// `0.0..1.0` across a single sweep: the band enters off the left edge and
/// leaves past the right one, which is why the travel spans the label plus a
/// band's width on either side.
fn shimmer_spans(label: &str, phase: f32, base: Rgb) -> Vec<PaintSpan> {
    shimmer_spans_with_band(label, phase, base, SHIMMER_BAND)
}

fn shimmer_spans_with_band(label: &str, phase: f32, base: Rgb, band: f32) -> Vec<PaintSpan> {
    let chars: Vec<char> = label.chars().collect();
    let travel = chars.len() as f32 + band * 2.0;
    let centre = phase.clamp(0.0, 1.0) * travel - band;
    chars
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let distance = (index as f32 - centre).abs() / band;
            // A raised cosine: full brightness under the centre, easing to
            // nothing at the band's edge with no visible seam either side.
            let level = if distance >= 1.0 {
                0.0
            } else {
                0.5 * (1.0 + (distance * std::f32::consts::PI).cos())
            };
            PaintSpan {
                text: ch.to_string(),
                tone: Tone::Shimmer(base, (level * 255.0).round() as u8),
                bold: false,
            }
        })
        .collect()
}

/// Gajae-Code's Unicode activity loader frames.
const WORKING_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Activity labels that carry the loader glyph and the shimmer sweep.
const SPINNING_ACTIVITY_LABELS: [&str; 2] = ["Working..", "Compacting.."];

/// Glyphs that head an activity row which is already over. A shimmer on these
/// reads as work still running, so they paint flat.
const SETTLED_ACTIVITY_GLYPHS: [&str; 2] = ["✓", "X"];

/// The loader glyph plus the space after it.
const WORKING_SPINNER_COLUMNS: usize = 2;
/// The label whose activity row also carries a progress bar.
const COMPACTING_LABEL: &str = "Compacting..";
/// How wide the bar is drawn when the terminal has the room for it.
const PROGRESS_TRACK_COLUMNS: usize = 20;
/// Below this the bar reads as a handful of blocks rather than a track, so the
/// row keeps the elapsed time alone instead.
const PROGRESS_TRACK_MINIMUM: usize = 8;

/// An indeterminate block enters from the left, crosses the track, and leaves
/// through the right edge. Providers expose only start/end, so this communicates
/// activity without pretending to know real progress.
fn progress_bar_spans(phase: f32, track: usize, tone: Tone) -> Vec<PaintSpan> {
    const BLOCK_COLUMNS: usize = 4;

    let block = track.min(BLOCK_COLUMNS);
    let travel = track + block;
    let offset = (phase.clamp(0.0, 0.999) * travel as f32).round() as isize - block as isize;
    let visible_start = offset.max(0) as usize;
    let visible_end = (offset + block as isize).clamp(0, track as isize) as usize;
    let visible = visible_end.saturating_sub(visible_start);
    let remaining = track.saturating_sub(visible_start + visible);
    let mut spans = Vec::new();
    if visible_start > 0 {
        spans.push(PaintSpan {
            text: "░".repeat(visible_start),
            tone: Tone::Muted,
            bold: false,
        });
    }
    if visible > 0 {
        spans.push(PaintSpan {
            text: "█".repeat(visible),
            tone,
            bold: false,
        });
    }
    if remaining > 0 {
        spans.push(PaintSpan {
            text: "░".repeat(remaining),
            tone: Tone::Muted,
            bold: false,
        });
    }
    spans
}

/// Test-only shorthand: the app always drives the spinner and its progress bar
/// from the same phase, so tests say it once.
#[cfg(test)]
fn activity_lines(
    activity: &str,
    activity_model: Option<&str>,
    phase: f32,
    width: u16,
) -> Vec<PaintLine> {
    activity_lines_with_progress(activity, activity_model, phase, phase, width)
}

fn activity_lines_with_progress(
    activity: &str,
    activity_model: Option<&str>,
    phase: f32,
    progress_phase: f32,
    width: u16,
) -> Vec<PaintLine> {
    let tone = activity_model.and_then(model_tone).unwrap_or(Tone::Plain);
    if UnicodeWidthStr::width(activity) > width.saturating_sub(2) as usize {
        return wrapped_line(" ", tone, activity, tone, false, width);
    }
    // `/compact` wears the same loader as a turn: it is a wait the user started
    // and cannot see progress on any other way.
    if let Some((label, trailer)) = SPINNING_ACTIVITY_LABELS.iter().find_map(|label| {
        activity
            .strip_prefix(label)
            .map(|trailer| (*label, trailer))
    }) {
        let shimmer_base = tone_rgb(tone).unwrap_or(theme::palette().foreground);
        let mut tail = vec![PaintSpan {
            text: format!(
                "{} ",
                WORKING_SPINNER[(phase.clamp(0.0, 0.999) * WORKING_SPINNER.len() as f32) as usize]
            ),
            tone,
            bold: false,
        }];
        // Working keeps its elapsed reading in the same sweep, so the active
        // state does not visually stop before the time at the right.
        let shimmer_text = if label == COMPACTING_LABEL {
            label.to_owned()
        } else {
            format!("{label}{trailer}")
        };
        tail.extend(shimmer_spans(&shimmer_text, phase, shimmer_base));
        if label == COMPACTING_LABEL {
            let spent = 1
                + WORKING_SPINNER_COLUMNS
                + UnicodeWidthStr::width(label)
                + UnicodeWidthStr::width(trailer)
                + 3;
            let track = usize::from(width)
                .saturating_sub(spent)
                .min(PROGRESS_TRACK_COLUMNS);
            if track >= PROGRESS_TRACK_MINIMUM {
                tail.push(PaintSpan {
                    text: " ".to_owned(),
                    tone,
                    bold: false,
                });
                tail.extend(progress_bar_spans(progress_phase, track, tone));
            }
            tail.push(PaintSpan {
                text: trailer.to_owned(),
                tone,
                bold: false,
            });
        }
        return vec![PaintLine {
            prefix: " ".to_owned(),
            prefix_tone: tone,
            text: String::new(),
            tone,
            bold: false,
            tool_heading: None,
            pick: None,
            tail,
        }];
    }
    if activity.starts_with("✧ Completed (") {
        return vec![PaintLine {
            prefix: " ".to_owned(),
            prefix_tone: tone,
            text: activity.to_owned(),
            tone,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        }];
    }
    let shimmer_base = tone_rgb(tone).unwrap_or(theme::palette().foreground);
    let (glyph, rest) = activity.split_once(' ').unwrap_or((activity, ""));
    let (label, trailer) = match rest.find('(') {
        // The space before the bracket belongs to the tail: a shimmer that sweeps
        // through it would stall for a beat on nothing.
        Some(index) => {
            let label = rest[..index].trim_end();
            (label, &rest[label.len()..])
        }
        None => (rest, ""),
    };
    let mut tail = if SETTLED_ACTIVITY_GLYPHS.contains(&glyph) {
        vec![PaintSpan {
            text: label.to_owned(),
            tone,
            bold: false,
        }]
    } else {
        shimmer_spans(label, phase, shimmer_base)
    };
    if !trailer.is_empty() {
        tail.push(PaintSpan {
            text: trailer.to_owned(),
            tone,
            bold: false,
        });
    }
    vec![PaintLine {
        prefix: " ".to_owned(),
        prefix_tone: tone,
        text: if rest.is_empty() {
            glyph.to_owned()
        } else {
            format!("{glyph} ")
        },
        tone,
        bold: false,
        tool_heading: None,
        pick: None,
        tail,
    }]
}

fn queue_preview_line(prompt: &str, index: usize, width: u16) -> PaintLine {
    PaintLine {
        prefix: " ".to_owned(),
        prefix_tone: Tone::Muted,
        text: "X".to_owned(),
        tone: Tone::Muted,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: vec![PaintSpan {
            text: format!(
                " Queue: {}",
                compact_right(prompt, usize::from(width).saturating_sub(11))
            ),
            tone: Tone::Muted,
            bold: false,
        }],
    }
    .with_picks(&[(0, Pick::RemoveQueuedPrompt(index))])
}

/// Running subagents are listed under the status line, one row each, so a fan-out
/// stays visible without pushing the transcript around. Rows disappear as each
/// subagent finishes; background rows may outlive the parent turn.
fn subagent_lines(subagents: &[SubagentView], width: u16) -> Vec<PaintLine> {
    subagents
        .iter()
        .enumerate()
        .map(|(index, subagent)| subagent_line(subagent, index, width))
        .collect()
}

fn subagent_line(subagent: &SubagentView, index: usize, width: u16) -> PaintLine {
    let elapsed = format!(" · {}", format_subagent_elapsed(subagent.elapsed.as_secs()));
    // The gutter, glyph, and elapsed reading are fixed, so only the name is
    // compacted when the terminal cannot hold the whole row.
    let reserved =
        1 + UnicodeWidthStr::width(SUBAGENT_GLYPH) + 1 + UnicodeWidthStr::width(elapsed.as_str());
    let available = usize::from(width).saturating_sub(reserved + 1);
    let name = compact_right(&subagent.name, available);

    PaintLine {
        prefix: " ".to_owned(),
        prefix_tone: Tone::Muted,
        text: SUBAGENT_GLYPH.to_owned(),
        tone: Tone::Accent,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: vec![
            PaintSpan {
                text: format!(" {name}"),
                tone: Tone::Plain,
                bold: false,
            },
            PaintSpan {
                text: elapsed,
                tone: Tone::Muted,
                bold: false,
            },
        ],
    }
    // The bullet and agent name open the same panel; the elapsed reading is left
    // alone so the row's right edge stays quiet.
    .with_picks(&[(0, Pick::Subagent(index)), (1, Pick::Subagent(index))])
}

fn format_subagent_elapsed(seconds: u64) -> String {
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

/// Matches the transcript's running-tool bullet so the composer rows read as the
/// same kind of activity.
const SUBAGENT_GLYPH: &str = "⏺";

fn queue_preview_lines(prompts: &[String], width: u16) -> Vec<PaintLine> {
    prompts
        .iter()
        .enumerate()
        .map(|(index, prompt)| queue_preview_line(prompt, index, width))
        .collect()
}

/// Places the full composer control strip at the right edge of a one-line
/// activity row. A narrow terminal keeps the controls on the composer rule,
/// where they can use their existing progressive compression.
fn activity_line_with_composer_controls(
    mut line: PaintLine,
    mode: &ComposerMode,
    notice: Option<&str>,
    width: u16,
) -> Option<PaintLine> {
    // Match the status row below: its leading gutter and this trailing hover
    // cell leave the terminal's final column empty, so both controls share an
    // edge without clipping Fast's right-hand hit area.
    let right_edge = (width as usize).saturating_sub(3);
    if let Some(notice) = notice {
        let available = right_edge.saturating_sub(painted_line_width(&line));
        let notice = compact_right(notice, available);
        if notice.is_empty() {
            return None;
        }
        line.tail.push(rule_gap(
            available.saturating_sub(UnicodeWidthStr::width(notice.as_str())),
        ));
        line.tail.push(PaintSpan {
            text: notice,
            tone: Tone::Accent,
            bold: false,
        });
        line.tail.push(rule_gap(1));
        return Some(line);
    }
    let available = right_edge
        .saturating_sub(painted_line_width(&line))
        .saturating_sub(COMPOSER_MODE_GAP);
    let badge = fitting_badge_spans(mode, available)?;
    let gap = right_edge - painted_line_width(&line) - spans_width(&badge.spans);
    let badge_start = line.tail.len() + 2;
    let mut picks = Vec::new();
    picks.extend(
        badge
            .shell_display_mode_index
            .map(|index| (badge_start + index, Pick::ShellDisplayMode)),
    );
    picks.extend(
        badge
            .diff_display_mode_index
            .map(|index| (badge_start + index, Pick::DiffDisplayMode)),
    );
    picks.extend(
        badge
            .response_length_index
            .map(|index| (badge_start + index, Pick::VibeMode)),
    );
    picks.extend(
        badge
            .fast_index
            .map(|index| (badge_start + index, Pick::FastMode)),
    );
    picks.extend(
        badge
            .permission_index
            .map(|index| (badge_start + index, Pick::ClaudePermissionMode)),
    );
    line.tail.push(rule_gap(gap));
    line.tail.extend(badge.spans);
    line.tail.push(rule_gap(1));
    Some(line.with_picks(&picks))
}

fn painted_line_text(line: &PaintLine) -> String {
    std::iter::once(line.prefix.as_str())
        .chain(std::iter::once(line.text.as_str()))
        .chain(
            line.tail
                .iter()
                .filter(|span| span.tone != Tone::CopyJoin)
                .map(|span| span.text.as_str()),
        )
        .collect()
}

fn painted_line_width(line: &PaintLine) -> usize {
    UnicodeWidthStr::width(painted_line_text(line).as_str())
}

/// An animated activity row changes only the shimmer tones. Repaint that leading
/// label in place so clearing the line does not make its elapsed tail blink.
#[allow(dead_code)]
fn shimmer_repaint_columns(previous: &PaintLine, current: &PaintLine) -> Option<Range<usize>> {
    if previous.prefix != current.prefix
        || previous.prefix_tone != current.prefix_tone
        || previous.text != current.text
        || previous.tone != current.tone
        || previous.bold != current.bold
        || previous.tool_heading != current.tool_heading
        || previous.pick != current.pick
        || previous.tail.len() != current.tail.len()
    {
        return None;
    }

    let count = current
        .tail
        .iter()
        .take_while(|span| matches!(span.tone, Tone::Shimmer(_, _)))
        .count();
    if count == 0
        || previous
            .tail
            .iter()
            .take_while(|span| matches!(span.tone, Tone::Shimmer(_, _)))
            .count()
            != count
    {
        return None;
    }

    if previous.tail[..count]
        .iter()
        .zip(&current.tail[..count])
        .any(|(previous, current)| {
            previous.text != current.text
                || previous.bold != current.bold
                || !matches!(previous.tone, Tone::Shimmer(_, _))
        })
        || previous.tail[count..] != current.tail[count..]
    {
        return None;
    }

    let start = UnicodeWidthStr::width(previous.prefix.as_str())
        + UnicodeWidthStr::width(previous.text.as_str());
    let end = start
        + current.tail[..count]
            .iter()
            .map(|span| UnicodeWidthStr::width(span.text.as_str()))
            .sum::<usize>();
    Some(start..end)
}

/// The composer frame is printable so it stays stable across terminal themes,
/// but its side rules, prompt gutter, and right-hand fill are not prompt text.
fn composer_content_columns(line: &PaintLine) -> Option<Range<usize>> {
    (line.prefix == "│ " && line.tail.last().is_some_and(|span| span.text == "│")).then(|| {
        let prefix_width = UnicodeWidthStr::width(line.prefix.as_str());
        let start = prefix_width + 2;
        let content_width = line
            .tail
            .first()
            .map(|span| UnicodeWidthStr::width(span.text.as_str()))
            .unwrap_or(0);
        start..start + content_width
    })
}

/// Composer chrome, code-box borders, and blank continuation gutters are not
/// text. Visible conversation markers are text and remain selectable so copy
/// and paste matches Claude Code.
fn selectable_content_columns(line: &PaintLine) -> Option<Range<usize>> {
    let content = bubble_content_columns(line);
    // 프롬프트의 세로선과 그 뒤 여백은 텍스트가 아니다. 본문이 빈 줄이라도 빈
    // 범위를 돌려주어야 하며, `None`으로 돌려보내면 제한이 없다는 뜻이 되어 그
    // 줄에서는 세로선까지 선택된다.
    if line.tone == Tone::UserPrompt {
        let start = UnicodeWidthStr::width(line.prefix.as_str());
        return Some(start..content.end.max(start));
    }
    let columns = composer_content_columns(line).or_else(|| {
        let boxed_code =
            line.prefix.ends_with("│ ") && line.tail.last().is_some_and(|span| span.text == "│");
        if boxed_code {
            let start = UnicodeWidthStr::width(line.prefix.as_str());
            let end = content.end.saturating_sub(1);
            return (start < end).then_some(start..end);
        }
        if let Some(indentation) = line
            .prefix
            .strip_suffix("• ")
            .or_else(|| line.prefix.strip_suffix("- "))
            && !indentation.is_empty()
            && indentation.chars().all(|ch| ch == ' ')
        {
            return Some(UnicodeWidthStr::width(indentation)..content.end);
        }
        let fallback_status_gutter =
            line.prefix == " " && line.prefix_tone == Tone::Muted && line.tone == Tone::Muted;
        let empty_gutter = !line.prefix.is_empty()
            && line.prefix.chars().all(|ch| ch == ' ')
            && !fallback_status_gutter;
        empty_gutter.then(|| UnicodeWidthStr::width(line.prefix.as_str())..content.end)
    });
    match columns {
        Some(columns) => {
            let start = columns.start.max(content.start);
            // `Some(start..start)` means this row deliberately has no selectable
            // cells.  Keep that distinction from `None`: a blank response row
            // still owns its continuation gutter, but that gutter is chrome.
            let end = columns.end.min(content.end).max(start);
            Some(start..end)
        }
        // A bubble row with no gutter of its own is still narrower than the band
        // it is painted on, so the padding has to be trimmed off here too.
        None => (content != (0..painted_line_width(line))).then_some(content),
    }
}

/// Columns a chat bubble row actually holds text in. Both bubbles are filled out
/// to a common width so their background paints as one band, and that filler is
/// chrome: a drag across it must not land in the clipboard.
fn bubble_content_columns(line: &PaintLine) -> Range<usize> {
    let width = painted_line_width(line);
    if line.tone == Tone::UserPrompt && CHAT_LAYOUT.load(Ordering::Relaxed) {
        let prefix = UnicodeWidthStr::width(line.prefix.as_str());
        let end = prefix + UnicodeWidthStr::width(line.text.trim_end());
        return prefix..end;
    }
    let filler = line
        .tail
        .iter()
        .rev()
        .take_while(|span| {
            span.tone == Tone::AssistantBubble && span.text.chars().all(|ch| ch == ' ')
        })
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum();
    0..width.saturating_sub(filler)
}

fn word_range_at(line: &CopyLine, column: usize) -> Option<Range<usize>> {
    let content = line
        .content_columns
        .clone()
        .unwrap_or(0..UnicodeWidthStr::width(line.text.as_str()));
    let mut cells = Vec::new();
    let mut start = 0;
    for ch in line.text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width > 0 && start >= content.start && start + width <= content.end {
            cells.push((ch, start, start + width));
        }
        start += width;
    }

    let selected = cells
        .iter()
        .position(|(_, start, end)| *start <= column && column < *end)?;
    if !is_word_char(cells[selected].0) {
        return None;
    }
    let mut first = selected;
    while first > 0 && is_word_char(cells[first - 1].0) {
        first -= 1;
    }
    let mut last = selected + 1;
    while last < cells.len() && is_word_char(cells[last].0) {
        last += 1;
    }
    Some(cells[first].1..cells[last - 1].2)
}

fn selection_columns_for_line(
    line: &PaintLine,
    range: CellRange,
    row: usize,
) -> Option<Range<usize>> {
    // A bubble's rounded edge rows are chrome, not text.
    if matches!(
        line.tone,
        Tone::AssistantBubbleHalf | Tone::UserPromptPadding
    ) {
        return None;
    }
    let mut selected = range.columns_for_row(row, painted_line_width(line))?;
    if let Some(content) = selectable_content_columns(line) {
        selected.start = selected.start.max(content.start);
        selected.end = selected.end.min(content.end);
    }
    (selected.start < selected.end).then_some(selected)
}

/// A drag paints only once it covers two characters. A click that wobbles by a
/// hair — or that lands inside a wide glyph and drifts to its other cell — is a
/// click, and flashing a highlight under a single character reads as noise
/// rather than feedback. Copying is untouched: `finish_selection` still hands
/// back whatever the drag covered.
fn selection_is_worth_painting(range: CellRange, lines: &[PaintLine]) -> bool {
    const MINIMUM: usize = 2;
    let mut count = 0;

    for row in range.start.row..=range.end.row {
        let Some(line) = lines.get(row) else {
            break;
        };
        let text = painted_line_text(line);
        let Some(columns) = selection_columns_for_line(line, range, row) else {
            continue;
        };
        count += selected_char_count(&text, &columns, MINIMUM - count);
        if count >= MINIMUM {
            return true;
        }
    }

    false
}

fn render_live_block_lines(
    live: &[LiveBlockView<'_>],
    width: u16,
    expanded_tools: &HashSet<u64>,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
) -> Vec<PaintLine> {
    visible_transcript_blocks(
        live.iter().map(|live| live.block),
        shell_display_mode,
        diff_display_mode,
    )
    .into_iter()
    .flat_map(|block| {
        block_group_lines(
            block,
            width,
            shell_display_mode,
            diff_display_mode,
            expanded_tools.contains(&block.id()),
        )
    })
    .collect()
}

#[cfg(test)]
fn normal_frame(
    live: &[Block],
    editor: &Editor,
    welcome: Option<WelcomeView>,
    suggestions: &[SuggestionView],
    activity: Option<&str>,
    status: StatusArea,
    width: u16,
) -> Frame {
    let live = live
        .iter()
        .map(|block| LiveBlockView { block, revision: 0 })
        .collect::<Vec<_>>();
    let live_lines = render_live_block_lines(
        &live,
        width,
        &HashSet::new(),
        ShellDisplayMode::Collapse,
        DiffDisplayMode::Collapse,
    );
    normal_frame_with_expansion(
        live_lines,
        editor,
        &[],
        &[],
        &[],
        "",
        welcome,
        suggestions,
        activity,
        None,
        0.5,
        0.5,
        status,
        width,
    )
}

#[allow(clippy::too_many_arguments)]
fn normal_frame_with_expansion(
    live_lines: Vec<PaintLine>,
    editor: &Editor,
    composer_images: &[String],
    queued_prompts: &[String],
    subagents: &[SubagentView],
    composer_placeholder: &str,
    welcome: Option<WelcomeView>,
    suggestions: &[SuggestionView],
    activity: Option<&str>,
    activity_model: Option<&str>,
    activity_phase: f32,
    activity_progress_phase: f32,
    status: StatusArea,
    width: u16,
) -> Frame {
    let mut lines = Vec::new();
    if let Some(welcome) = welcome {
        lines.extend(welcome_lines(welcome, width));
        lines.push(PaintLine::blank());
    }
    lines.extend(live_lines);

    let mut dock_index = lines.len();
    let composer_mode = status.composer_mode.as_ref();
    // During a response, every transient notice uses the same right-hand slot.
    let activity_composer_notice = status.composer_notice.as_deref();
    let mut composer_notice = status.composer_notice.as_deref();
    let mut composer_controls_mode = composer_mode;
    let activity_uses_composer_spacer = activity.is_some() && suggestions.is_empty();
    let idle_controls_can_use_composer_spacer =
        activity.is_none() && suggestions.is_empty() && composer_mode.is_some();
    let mut activity_index = None;
    // Transient rows stay in the pinned dock instead of scrolling away with the
    // conversation. Activity leads any command suggestions.
    if let Some(activity) = activity {
        if !matches!(lines.last(), Some(line) if line == &PaintLine::blank()) {
            lines.push(PaintLine::blank());
        }
        let mut activity_rows = activity_lines_with_progress(
            activity,
            activity_model,
            activity_phase,
            activity_progress_phase,
            width,
        );
        if let Some(mode) = composer_controls_mode
            && let Some(row) = activity_line_with_composer_controls(
                activity_rows[0].clone(),
                mode,
                activity_composer_notice,
                width,
            )
        {
            activity_rows[0] = row;
            if activity_composer_notice.is_some() {
                composer_notice = None;
            }
            // The active row now carries the controls, so the composer rule
            // leaves them alone rather than painting a second copy.
            composer_controls_mode = None;
        }
        // A long activity label leaves no room beside it. The controls stay on
        // the composer rule in that case instead of vanishing for the whole
        // turn — they are clickable settings, not decoration.
        activity_index = Some(lines.len());
        lines.extend(activity_rows);
        if !suggestions.is_empty() {
            lines.push(PaintLine::blank());
        }
    }
    if !suggestions.is_empty() {
        lines.extend(suggestion_lines(suggestions, width));
    }
    // Keep the spacer directly above the composer reserved so a notice appearing
    // on the bottom rule never resizes the frame or moves the transcript.
    if lines.last() == Some(&PaintLine::blank()) {
        lines.pop();
        dock_index = dock_index.min(lines.len());
    }
    if !activity_uses_composer_spacer {
        if idle_controls_can_use_composer_spacer
            && let Some(mode) = composer_controls_mode
            && let Some(row) =
                activity_line_with_composer_controls(PaintLine::blank(), mode, None, width)
        {
            lines.push(row);
            composer_controls_mode = None;
        } else {
            lines.push(PaintLine::blank());
        }
    }
    lines.extend(queue_preview_lines(queued_prompts, width));

    // Recalled history is labelled on the composer rule, so the position stays
    // visible for as long as the entry does.
    let recalled = editor
        .history_position()
        .map(|(position, total)| format!("{position}/{total}"))
        .unwrap_or_default();
    let (input_lines, input_cursor_line, input_cursor_col, composer_layout) =
        input_lines_with_controls(
            editor,
            composer_images,
            width,
            &recalled,
            composer_placeholder,
            composer_notice,
            composer_mode,
            composer_controls_mode,
        );
    let composer_index = lines.len();
    let cursor_line = composer_index + input_cursor_line;
    lines.extend(input_lines);
    if status.fallback != HIDDEN_STATUS_LINE {
        lines.push(status_line_row(status.line, &status.fallback, width));
    }
    lines.extend(subagent_lines(subagents, width));

    Frame {
        lines,
        cursor_line,
        cursor_col: input_cursor_col,
        show_cursor: true,
        dock_index,
        composer_index: Some(composer_index),
        composer_layout: Some(composer_layout),
        activity_index,
    }
}

/// The welcome card is deliberately two rows: the product headline and the
/// working folder. Everything else lives behind `/help` and the status line.
/// One blank row leads it so the headline never sits on the terminal's top edge.
fn welcome_lines(welcome: WelcomeView, width: u16) -> Vec<PaintLine> {
    let column_width = panel_span(width);
    vec![
        PaintLine::blank(),
        plain_line(
            &format!("DEVEZ VIBE  v{}", crate::update::CURRENT_VERSION),
            Tone::Accent,
            true,
        ),
        plain_line(
            &compact_text(&welcome.cwd, column_width),
            Tone::Muted,
            false,
        ),
    ]
}

fn plain_line(text: &str, tone: Tone, bold: bool) -> PaintLine {
    PaintLine {
        prefix: String::new(),
        prefix_tone: tone,
        text: text.to_owned(),
        tone,
        bold,
        tool_heading: None,
        pick: None,
        tail: Vec::new(),
    }
}

/// The rule that separates the answers from the row that walks away from them.
fn question_rule_row(panel_width: usize) -> PaintLine {
    PaintLine {
        prefix: "├".to_owned(),
        prefix_tone: Tone::Border,
        text: "─".repeat(panel_width.saturating_sub(2)),
        tone: Tone::Border,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: vec![PaintSpan {
            text: "┤".to_owned(),
            tone: Tone::Border,
            bold: false,
        }],
    }
}

/// Panels share the composer's span so their borders line up with the rule.
fn panel_span(width: u16) -> usize {
    (width as usize).saturating_sub(1).max(19)
}

/// A titled rule such as `╭─ Sign in ────╮`, closed with `corner`.
/// The mark a closable panel wears on its top rule, and the columns it spends:
/// a blank either side of the mark plus the stroke of rule left between it and the
/// corner, so the `X` reads as sitting inside the box rather than replacing its
/// edge. The blanks are the mark's own target, being what the click region reaches
/// into either side.
const CLOSE_MARK: &str = "X";
const CLOSE_RESERVED: usize = 4;
/// Paint-order position of the mark once a rule row has been split around it.
const CLOSE_SPAN: usize = 2;

/// The rule that runs from a panel's label to its corner, split around the close
/// mark when the panel carries one. A rule with no room for the mark keeps it off
/// rather than shrinking the label.
fn rule_tail_spans(rule_width: usize, corner: char, closable: bool) -> Vec<PaintSpan> {
    let border = |text: String| PaintSpan {
        text,
        tone: Tone::Border,
        bold: false,
    };
    if !closable || rule_width <= CLOSE_RESERVED {
        return vec![border(format!("{}{corner}", "─".repeat(rule_width)))];
    }
    vec![
        border(format!("{} ", "─".repeat(rule_width - CLOSE_RESERVED))),
        PaintSpan {
            text: CLOSE_MARK.to_owned(),
            tone: Tone::Muted,
            bold: false,
        },
        border(format!(" ─{corner}")),
    ]
}

fn panel_rule_row(opening: &str, label: &str, corner: char, panel_width: usize) -> PaintLine {
    panel_rule_row_closable(opening, label, corner, panel_width, false)
}

fn panel_rule_row_closable(
    opening: &str,
    label: &str,
    corner: char,
    panel_width: usize,
    closable: bool,
) -> PaintLine {
    let label = compact_right(label, panel_width.saturating_sub(6));
    let used = UnicodeWidthStr::width(opening)
        + UnicodeWidthStr::width(label.as_str())
        + 1  // the space after the label
        + 1; // the closing corner
    PaintLine {
        prefix: opening.to_owned(),
        prefix_tone: Tone::Border,
        text: format!("{label} "),
        tone: if corner == '╮' {
            Tone::Accent
        } else {
            Tone::Muted
        },
        bold: corner == '╮',
        tool_heading: None,
        pick: None,
        tail: rule_tail_spans(panel_width.saturating_sub(used), corner, closable),
    }
    .with_picks(&[(CLOSE_SPAN, Pick::Close)])
}

fn panel_title_row(title: &str, panel_width: usize, closable: bool) -> PaintLine {
    let header = format!("{title} ");
    let header_rule = panel_width
        .saturating_sub(3 + UnicodeWidthStr::width(header.as_str()) + 1)
        .max(1);
    PaintLine {
        prefix: "╭─ ".to_owned(),
        prefix_tone: Tone::Border,
        text: header,
        tone: Tone::Muted,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: rule_tail_spans(header_rule, '╮', closable),
    }
    .with_picks(&[(CLOSE_SPAN, Pick::Close)])
}

/// Pads a wrapped panel row out to `panel_width` and caps it with `│`.
fn close_panel_row(mut line: PaintLine, panel_width: usize) -> PaintLine {
    let used = UnicodeWidthStr::width(line.prefix.as_str())
        + UnicodeWidthStr::width(line.text.as_str())
        + line
            .tail
            .iter()
            .map(|span| UnicodeWidthStr::width(span.text.as_str()))
            .sum::<usize>();
    let padding = panel_width.saturating_sub(used + 1);
    line.tail.push(PaintSpan {
        text: format!("{}│", " ".repeat(padding)),
        tone: Tone::Border,
        bold: false,
    });
    line
}

/// The row's leading `│` belongs to the panel, not to the row. Toning the whole
/// prefix with the selection painted that border in the accent as well, so the
/// box read as though its own edge were the thing being picked. Keep the border
/// on the panel's tone and let only the marker follow the selection.
fn split_panel_border(mut line: PaintLine, marker_tone: Tone) -> PaintLine {
    let Some(marker) = line.prefix.strip_prefix('│').map(ToOwned::to_owned) else {
        return line;
    };
    let label = PaintSpan {
        text: std::mem::take(&mut line.text),
        tone: line.tone,
        bold: line.bold,
    };
    line.prefix = "│".to_owned();
    line.prefix_tone = Tone::Border;
    line.text = marker;
    line.tone = marker_tone;
    line.bold = false;
    line.tail.insert(0, label);
    line
}

fn panelize_content_line(mut line: PaintLine, panel_width: usize) -> PaintLine {
    line.prefix.insert(0, '│');
    line.prefix_tone = Tone::Border;
    // The border pushes every column of the row along with it, and the clickable
    // spans were measured before it went on.
    line.pick = line.pick.map(|regions| regions.shifted(1));
    close_panel_row(line, panel_width)
}

fn panel_bottom(inner_width: usize) -> PaintLine {
    PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: format!("╰{}╯", "─".repeat(inner_width)),
        tone: Tone::Border,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: Vec::new(),
    }
}

/// Release notes use the same compact heading-and-rows rhythm as the task list.
fn update_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let max_width = panel_span(width);
    let line_width = if is_startup_update(block) {
        let title_width = UnicodeWidthStr::width(block.title.as_str()) + 6;
        let body_width = block
            .body
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or_default()
            + 15;
        title_width.max(body_width).min(max_width)
    } else {
        max_width
    };
    let title = compact_text(&block.title, line_width.saturating_sub(7));
    let title_width = UnicodeWidthStr::width(title.as_str());
    let rule = "─".repeat(line_width.saturating_sub(6 + title_width));
    let mut lines = vec![
        PaintLine {
            tone: PLAN_BORDER_TONE,
            ..PaintLine::plain(format!("┌── {title} {rule}┐"))
        },
        PaintLine::blank(),
    ];
    let wrap_width = if is_startup_update(block) {
        line_width.saturating_sub(1).min(u16::MAX as usize) as u16
    } else {
        width
    };
    for note in block.body.lines().filter(|line| !line.trim().is_empty()) {
        lines.extend(wrapped_line(
            "  •  ",
            Tone::Muted,
            note,
            Tone::Muted,
            false,
            wrap_width,
        ));
    }
    lines.push(PaintLine::blank());
    lines.push(PaintLine {
        tone: PLAN_BORDER_TONE,
        ..PaintLine::plain(format!("└{}┘", "─".repeat(line_width.saturating_sub(2))))
    });
    lines.push(PaintLine::blank());
    lines
}

fn is_startup_update(block: &Block) -> bool {
    matches!(block.kind, BlockKind::Update) && block.title == "Tip"
}

fn suggestion_lines(suggestions: &[SuggestionView], width: u16) -> Vec<PaintLine> {
    let panel_width = panel_span(width);
    let inner_width = panel_width.saturating_sub(2);
    let title = suggestions
        .first()
        .map(|suggestion| suggestion.panel_title)
        .unwrap_or("Commands");
    let mut lines = vec![panel_title_row(title, panel_width, false)];
    lines.push(panel_padding_row(panel_width));
    for suggestion in &suggestions[visible_window(
        suggestions.iter().position(|item| item.selected),
        suggestions.len(),
        SUGGESTION_ROWS,
    )] {
        let marker = if suggestion.selected { "❯" } else { " " };
        let content = match suggestion.category.as_deref() {
            Some(category) if suggestion.description.is_empty() => {
                format!(" {marker} [{category}] {}", suggestion.command)
            }
            Some(category) => format!(
                " {marker} [{category}] {}  {}",
                suggestion.command, suggestion.description
            ),
            None => format!(
                " {marker} {:<COMMAND_COLUMN_WIDTH$} {}",
                suggestion.command, suggestion.description
            ),
        };
        lines.push(panel_line_keep_left(
            &content,
            panel_width,
            if suggestion.selected {
                Tone::Accent
            } else {
                Tone::Muted
            },
            suggestion.selected,
        ));
    }
    if let Some(hint) = suggestions
        .iter()
        .find_map(|suggestion| suggestion.hint.as_deref())
    {
        lines.push(panel_line_keep_left(
            &format!("   {hint}"),
            panel_width,
            Tone::Muted,
            false,
        ));
    }
    lines.push(panel_padding_row(panel_width));
    lines.push(panel_bottom(inner_width));
    lines
}

/// Rows the command dock shows at once. Longer lists scroll.
const SUGGESTION_ROWS: usize = 6;
const COMMAND_COLUMN_WIDTH: usize = 16;

/// The slice of a list to draw so `selected` is always on screen, keeping a
/// third of the rows above it as context and never leaving a short window at
/// the end of a list that could fill one.
/// Rows a full-screen picker lists at once. Longer catalogues scroll.
pub const PICKER_ROWS: usize = 9;

pub fn visible_window(selected: Option<usize>, len: usize, rows: usize) -> std::ops::Range<usize> {
    let last_start = len.saturating_sub(rows);
    let start = selected
        .unwrap_or(0)
        .saturating_sub(rows / 3)
        .min(last_start);
    start..(start + rows).min(len)
}

/// A bordered blank row. Every boxed list gets one under its top rule and one
/// above its bottom rule, so the contents never sit flush against the border.
fn panel_padding_row(panel_width: usize) -> PaintLine {
    panel_line("", panel_width, Tone::Muted, false)
}

fn panel_line(text: &str, width: usize, tone: Tone, bold: bool) -> PaintLine {
    panel_line_with(text, width, tone, bold, compact_text)
}

fn panel_line_keep_left(text: &str, width: usize, tone: Tone, bold: bool) -> PaintLine {
    panel_line_with(text, width, tone, bold, compact_right)
}

/// A left-anchored row that keeps `right_inset` blank columns before the right
/// border, so a truncated row never crowds the box.
fn panel_line_keep_left_inset(
    text: &str,
    width: usize,
    right_inset: usize,
    tone: Tone,
    bold: bool,
) -> PaintLine {
    let inner_width = width.saturating_sub(2);
    let inset = right_inset.min(inner_width);
    let content_width = inner_width.saturating_sub(inset);
    let single_line = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    let content = if content_width == 0 {
        String::new()
    } else {
        compact_right(&single_line, content_width)
    };
    panel_line(
        &format!("{content}{}", " ".repeat(inset)),
        width,
        tone,
        bold,
    )
}

fn panel_line_with(
    text: &str,
    width: usize,
    tone: Tone,
    bold: bool,
    compact: fn(&str, usize) -> String,
) -> PaintLine {
    let inner_width = width.saturating_sub(2);
    let single_line = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    let content = compact(&single_line, inner_width);
    let padding = inner_width.saturating_sub(UnicodeWidthStr::width(content.as_str()));
    PaintLine {
        prefix: "│".to_owned(),
        prefix_tone: Tone::Border,
        text: format!("{content}{}", " ".repeat(padding)),
        tone,
        bold,
        tool_heading: None,
        pick: None,
        tail: vec![PaintSpan {
            text: "│".to_owned(),
            tone: Tone::Border,
            bold: false,
        }],
    }
}

const EFFORT_SEPARATOR: &str = " › ";

/// Effort reads as a sequence of compute steps rather than a speed-to-depth
/// axis. The selected step stays fully named while the others compact on narrow
/// terminals.
#[allow(dead_code)]
fn effort_step_lines(slider: &EffortSlider, width: u16) -> Vec<PaintLine> {
    effort_step_lines_in(slider, panel_span(width))
}

fn effort_step_lines_in(slider: &EffortSlider, inner: usize) -> Vec<PaintLine> {
    if slider.efforts.is_empty() {
        return Vec::new();
    }

    let selected = slider.selected.min(slider.efforts.len() - 1);
    let full = effort_step_spans(slider, selected, false);
    let full_width = steps_width(&full);
    let mut spans = if full_width <= inner {
        full
    } else {
        effort_step_spans(slider, selected, true)
    };
    while steps_width(&spans) > inner {
        let Some(step) = spans
            .iter()
            .rposition(|step| !step.span.bold && step.effort.is_some())
        else {
            break;
        };
        if step + 1 < spans.len() {
            spans.drain(step..step + 2);
        } else {
            spans.drain(step.saturating_sub(1)..=step);
        }
    }
    let content_width = steps_width(&spans);
    let indent = inner.saturating_sub(content_width) / 2;
    let selected_span = spans
        .iter()
        .position(|step| step.span.bold)
        .expect("non-empty effort list has a selection");
    let selected_offset = steps_width(&spans[..selected_span]);
    let selected_width = UnicodeWidthStr::width(spans[selected_span].span.text.as_str());
    let selected_tone = spans[selected_span].span.tone;
    let border_prefix = " ".repeat(indent + selected_offset);
    let border_fill = "─".repeat(selected_width.saturating_sub(2));
    // The step a click on this row lands on. The row opens with an empty `text`,
    // so the steps start one span in.
    let picks = spans
        .iter()
        .enumerate()
        .filter_map(|(index, step)| step.effort.map(|effort| (index + 1, Pick::Effort(effort))))
        .collect::<Vec<_>>();
    let spans = spans.into_iter().map(|step| step.span).collect::<Vec<_>>();

    vec![
        PaintLine {
            prefix: border_prefix.clone(),
            prefix_tone: Tone::Muted,
            text: format!("╭{border_fill}╮"),
            tone: selected_tone,
            bold: true,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        },
        PaintLine {
            prefix: " ".repeat(indent),
            prefix_tone: Tone::Muted,
            text: String::new(),
            tone: Tone::Muted,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: spans,
        }
        .with_picks(&picks),
        PaintLine {
            prefix: border_prefix,
            prefix_tone: Tone::Muted,
            text: format!("╰{border_fill}╯"),
            tone: selected_tone,
            bold: true,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        },
    ]
}

/// A painted piece of the effort track and the step it stands for. The
/// separators between the steps stand for nothing, and the shrinking that fits
/// the track into a narrow panel drops steps, so which piece is which step is
/// only knowable while the track is being built.
struct EffortStepSpan {
    span: PaintSpan,
    effort: Option<usize>,
}

fn steps_width(steps: &[EffortStepSpan]) -> usize {
    steps
        .iter()
        .map(|step| UnicodeWidthStr::width(step.span.text.as_str()))
        .sum()
}

fn effort_step_spans(slider: &EffortSlider, selected: usize, compact: bool) -> Vec<EffortStepSpan> {
    let selected_tone = slider
        .efforts
        .get(selected)
        .and_then(|effort| effort_tone(effort))
        .unwrap_or(Tone::Accent);
    let mut spans = Vec::with_capacity(slider.efforts.len() * 2 - 1);

    for (index, effort) in slider.efforts.iter().enumerate() {
        if index > 0 {
            spans.push(EffortStepSpan {
                span: PaintSpan {
                    text: EFFORT_SEPARATOR.to_owned(),
                    tone: if index == selected + 1 {
                        selected_tone
                    } else {
                        Tone::Muted
                    },
                    bold: false,
                },
                effort: None,
            });
        }

        let is_selected = index == selected;
        spans.push(EffortStepSpan {
            span: PaintSpan {
                text: effort_step_label(effort, is_selected, compact),
                tone: if is_selected {
                    selected_tone
                } else {
                    Tone::Muted
                },
                bold: is_selected,
            },
            effort: Some(index),
        });
    }

    spans
}

fn effort_step_label(effort: &str, selected: bool, compact: bool) -> String {
    if selected {
        return format!("│ {} │", effort.to_ascii_uppercase());
    }
    if !compact {
        return effort.to_owned();
    }

    match effort {
        "low" => "L",
        "medium" => "M",
        "high" => "H",
        "xhigh" => "XH",
        "max" => "MAX",
        "ultra" | "ultracode" => "U",
        unknown => unknown,
    }
    .to_owned()
}

#[cfg(test)]
fn overlay_frame(
    live: &[Block],
    overlay: OverlayView<'_>,
    welcome: Option<WelcomeView>,
    status: StatusArea,
    width: u16,
) -> Frame {
    let live = live
        .iter()
        .map(|block| LiveBlockView { block, revision: 0 })
        .collect::<Vec<_>>();
    let live_lines = render_live_block_lines(
        &live,
        width,
        &HashSet::new(),
        ShellDisplayMode::Collapse,
        DiffDisplayMode::Collapse,
    );
    overlay_frame_with_expansion(live_lines, overlay, welcome, status, width)
}

fn overlay_frame_with_expansion(
    live_lines: Vec<PaintLine>,
    overlay: OverlayView<'_>,
    welcome: Option<WelcomeView>,
    status: StatusArea,
    width: u16,
) -> Frame {
    let mut lines = Vec::new();
    // A picker docks over the transcript rather than replacing the screen, so the
    // welcome card stays where it was instead of blinking out for as long as
    // `/model`, `/effort` or `/resume` is open. On a short terminal `fit_frame`
    // trims it from the top, the same way it does under the normal frame.
    if let Some(welcome) = welcome {
        lines.extend(welcome_lines(welcome, width));
        lines.push(PaintLine::blank());
    }
    lines.extend(live_lines);
    let dock_index = lines.len();
    // Set when a free-text answer is typed on the option row it was picked on,
    // which is where the cursor then belongs.
    let mut inline_cursor = None;

    match overlay.style {
        OverlayStyle::Picker => {
            let panel_width = panel_span(width);
            // Keep one ordinary terminal cell open before the closing border;
            // the effort track below uses the same narrower content area.
            let inner_width = panel_width.saturating_sub(3);
            lines.push(panel_title_row(
                &overlay.title,
                panel_width,
                overlay.closable,
            ));
            lines.push(panel_padding_row(panel_width));

            for (row_index, row) in overlay.lines.iter().enumerate() {
                if row.text.is_empty() {
                    lines.push(panel_padding_row(panel_width));
                    continue;
                }
                for (part_index, part) in row.text.lines().enumerate() {
                    let prefix = if part_index == 0 {
                        if row.selected { "│ ❯ " } else { "│   " }
                    } else {
                        "│     "
                    };
                    // A detail line folds back under itself, not under the label
                    // above it, so its own indent is the continuation indent.
                    let continuation = if part_index == 0 {
                        "│   "
                    } else {
                        "│     "
                    };
                    let tone = if row.muted {
                        Tone::Muted
                    } else if part.contains('●') && part.contains('○') {
                        Tone::Accent
                    } else {
                        model_tone(part).unwrap_or(Tone::Plain)
                    };
                    let wrapped = wrapped_line_with_continuation(
                        prefix,
                        continuation,
                        Tone::Border,
                        part,
                        tone,
                        row.selected && part_index == 0,
                        (panel_width.saturating_sub(2)).min(u16::MAX as usize) as u16,
                    );
                    lines.extend(wrapped.into_iter().map(|line| {
                        let line = close_panel_row(line, panel_width);
                        // A muted row is the picker talking, not offering: a
                        // summary or a heading answers to no click, so it must
                        // not light up as though it did. What is on offer is the
                        // row's own text — the panel borders and the padding out
                        // to them are furniture, and highlighting them would read
                        // as the box itself being pressed.
                        if row.muted {
                            line
                        } else {
                            line.with_picks(&[(0, Pick::Row(row_index))])
                        }
                    }));
                }
            }
            if let Some(slider) = overlay.slider {
                lines.extend(
                    effort_step_lines_in(&slider, inner_width)
                        .into_iter()
                        .map(|line| panelize_content_line(line, panel_width)),
                );
                if let Some(detail) = slider.detail.as_deref() {
                    lines.push(panel_padding_row(panel_width));
                    lines.extend(
                        wrapped_line_with_continuation(
                            "│   ",
                            "│   ",
                            Tone::Border,
                            detail,
                            Tone::Muted,
                            false,
                            (panel_width.saturating_sub(1)).min(u16::MAX as usize) as u16,
                        )
                        .into_iter()
                        .map(|line| close_panel_row(line, panel_width)),
                    );
                }
            }
            lines.push(panel_padding_row(panel_width));
            lines.push(panel_rule_row("╰─ ", &overlay.hint, '╯', panel_width));
        }
        OverlayStyle::CompactPanel => {
            /// `" ❯ "`: what a compact row spends before its own text starts.
            const COMPACT_ROW_GUTTER_COLUMNS: usize = 3;
            /// Blank columns kept before the right border so a truncated row
            /// never crowds the box.
            const COMPACT_ROW_RIGHT_INSET: usize = 3;

            let panel_width = panel_span(width);
            lines.push(panel_rule_row_closable(
                "╭─ ",
                &overlay.title,
                '╮',
                panel_width,
                overlay.closable,
            ));
            lines.push(panel_padding_row(panel_width));
            for (row_index, row) in overlay.lines.iter().enumerate() {
                let marker = if row.selected { "❯" } else { " " };
                let mut line = panel_line_keep_left_inset(
                    &format!(" {marker} {}", row.text),
                    panel_width,
                    COMPACT_ROW_RIGHT_INSET,
                    if row.selected {
                        Tone::Accent
                    } else if row.muted {
                        Tone::Muted
                    } else {
                        Tone::Plain
                    },
                    row.selected,
                );
                if !row.muted {
                    // The same span the taller pickers offer: the row's own text,
                    // never the marker gutter, the borders, or the padding out to
                    // them. Those are furniture, and lighting them up would read
                    // as the box itself being pressed.
                    let start =
                        UnicodeWidthStr::width(line.prefix.as_str()) + COMPACT_ROW_GUTTER_COLUMNS;
                    let end = UnicodeWidthStr::width(line.prefix.as_str())
                        + UnicodeWidthStr::width(line.text.trim_end());
                    if end > start {
                        line.pick = Some(PickRegions::span(start, end, Pick::Row(row_index)));
                    }
                }
                lines.push(line);
            }
            lines.push(panel_padding_row(panel_width));
            lines.push(panel_rule_row("╰─ ", &overlay.hint, '╯', panel_width));
        }
        OverlayStyle::Panel => {
            // A closed box: every row lands on exactly `panel_width` columns.
            let panel_width = panel_span(width);
            lines.push(panel_rule_row_closable(
                "╭─ ",
                &overlay.title,
                '╮',
                panel_width,
                overlay.closable,
            ));
            lines.push(panel_padding_row(panel_width));
            for (row_index, row) in overlay.lines.iter().enumerate() {
                // An empty row is padding of the caller's own. It still needs
                // both borders, so it cannot go through the wrapping path,
                // which yields no rows at all for empty text.
                if row.text.is_empty() {
                    lines.push(panel_padding_row(panel_width));
                    continue;
                }
                for (part_index, part) in row.text.lines().enumerate() {
                    let prefix = if part_index == 0 {
                        if row.selected { "│ ❯ " } else { "│   " }
                    } else {
                        "│     "
                    };
                    // A detail line folds back under itself, not under the label
                    // above it, so its own indent is the continuation indent.
                    let continuation = if part_index == 0 {
                        "│   "
                    } else {
                        "│     "
                    };
                    // The line under a label is that label's detail, not a claim
                    // of its own: it reads as the quieter half of one row.
                    let tone = if row.muted || part_index > 0 {
                        Tone::Muted
                    } else {
                        Tone::Plain
                    };
                    // Reserve the closing border before wrapping, not after.
                    let wrapped = wrapped_line_with_continuation(
                        prefix,
                        continuation,
                        Tone::Border,
                        part,
                        tone,
                        row.selected && part_index == 0,
                        (panel_width.saturating_sub(1)).min(u16::MAX as usize) as u16,
                    );
                    lines.extend(wrapped.into_iter().map(|line| {
                        let line = close_panel_row(line, panel_width);
                        // A muted row is the picker talking, not offering: a
                        // summary or a heading answers to no click, so it must
                        // not light up as though it did. What is on offer is the
                        // row's own text — the panel borders and the padding out
                        // to them are furniture, and highlighting them would read
                        // as the box itself being pressed.
                        if row.muted {
                            line
                        } else {
                            line.with_picks(&[(0, Pick::Row(row_index))])
                        }
                    }));
                }
            }
            lines.push(panel_padding_row(panel_width));
            lines.push(panel_rule_row("╰─ ", &overlay.hint, '╯', panel_width));
        }
        OverlayStyle::Question => {
            let panel_width = panel_span(width);
            let wrap_width = panel_width.saturating_sub(1).min(u16::MAX as usize) as u16;
            lines.push(panel_title_row(
                &overlay.title,
                panel_width,
                overlay.closable,
            ));
            lines.push(panel_padding_row(panel_width));

            let mut rows = overlay.lines.iter().enumerate();
            if let Some((_, prompt)) = rows.next() {
                lines.extend(
                    wrapped_line_with_continuation(
                        "│   ",
                        "│   ",
                        Tone::Border,
                        &prompt.text,
                        Tone::Plain,
                        true,
                        wrap_width,
                    )
                    .into_iter()
                    .map(|mut line| {
                        line.bold = true;
                        close_panel_row(line, panel_width)
                    }),
                );
                lines.push(panel_padding_row(panel_width));
            }

            // Options are numbered from one, and the number column is as wide as
            // the last number so every label starts on the same column.
            let option_count = overlay.lines.len().saturating_sub(1);
            let number_width = option_count.max(1).to_string().len();
            let label_column = 6 + number_width;
            let continuation = format!("│{}", " ".repeat(label_column.saturating_sub(1)));
            let last = overlay.lines.len().saturating_sub(1);
            // A question that still lists its options types the free-text answer on
            // the row it was picked on. Only a question with nothing to pick from
            // falls back to the box under the panel.
            let inline_input = overlay.input.filter(|_| overlay.lines.len() > 1);
            for (row_index, row) in rows {
                // The final row leaves the question rather than answering it, so
                // a rule sets it apart from the answers above.
                if row_index == last && option_count > 1 {
                    lines.push(question_rule_row(panel_width));
                }
                let number = row_index;
                if let Some(editor) = inline_input.filter(|_| row.selected) {
                    // The selected row already identifies the free-text choice.
                    // Keep every cell after its number empty until text commits,
                    // so no label or placeholder can sit under the IME preedit.
                    let prefix = format!("│ ❯ {number:>number_width$}. ");
                    let (rows_text, cursor_row, cursor_column) = inline_answer_rows(
                        editor,
                        UnicodeWidthStr::width(prefix.as_str()),
                        wrap_width,
                    );
                    inline_cursor = Some((lines.len() + cursor_row, cursor_column));
                    for (part_index, part) in rows_text.iter().enumerate() {
                        let line_prefix = if part_index == 0 {
                            prefix.clone()
                        } else {
                            continuation.clone()
                        };
                        lines.extend(
                            wrapped_line_with_continuation(
                                &line_prefix,
                                &continuation,
                                Tone::Border,
                                part,
                                Tone::Plain,
                                false,
                                wrap_width,
                            )
                            .into_iter()
                            .map(|line| {
                                close_panel_row(split_panel_border(line, Tone::Accent), panel_width)
                                    .with_picks(&[(0, Pick::Row(row_index))])
                            }),
                        );
                    }
                    continue;
                }
                for (part_index, part) in row.text.lines().enumerate() {
                    let prefix = if part_index == 0 {
                        format!(
                            "│ {} {number:>number_width$}. ",
                            if row.selected { "❯" } else { " " }
                        )
                    } else {
                        continuation.clone()
                    };
                    let tone = if part_index > 0 || row.muted {
                        Tone::Muted
                    } else if row.selected {
                        Tone::Accent
                    } else {
                        Tone::Plain
                    };
                    lines.extend(
                        wrapped_line_with_continuation(
                            &prefix,
                            &continuation,
                            Tone::Border,
                            part,
                            tone,
                            part_index == 0 && !row.muted,
                            wrap_width,
                        )
                        .into_iter()
                        .map(|line| {
                            let marker = if row.selected {
                                Tone::Accent
                            } else {
                                Tone::Muted
                            };
                            close_panel_row(split_panel_border(line, marker), panel_width)
                                .with_picks(&[(0, Pick::Row(row_index))])
                        }),
                    );
                }
            }
            lines.push(panel_padding_row(panel_width));
            lines.push(panel_rule_row("╰─ ", &overlay.hint, '╯', panel_width));
        }
    }
    let mut cursor_line = lines.len() - 1;
    let mut cursor_col = 0;
    let mut composer_index = None;
    let show_cursor = if let Some((line, column)) = inline_cursor {
        cursor_line = line;
        cursor_col = column;
        true
    } else if let Some(editor) = overlay.input {
        // The composer rule reads as part of the picker without this gap.
        lines.push(PaintLine::blank());
        // A picker's own input is not the composer, so a drag over it stays a
        // copy: there is no prompt buffer behind it for a delete to reach.
        let (input, input_cursor_line, input_cursor_col, _) = input_lines(
            editor,
            &[],
            width,
            overlay.input_label,
            overlay.input_placeholder,
            None,
            status.composer_mode.as_ref(),
        );
        composer_index = Some(lines.len());
        cursor_line = lines.len() + input_cursor_line;
        cursor_col = input_cursor_col;
        lines.extend(input);
        true
    } else {
        false
    };
    if status.fallback != HIDDEN_STATUS_LINE {
        lines.push(PaintLine::blank());
        lines.push(status_line_row(status.line, &status.fallback, width));
    }

    Frame {
        cursor_line,
        cursor_col,
        lines,
        show_cursor,
        dock_index,
        composer_index,
        composer_layout: None,
        activity_index: None,
    }
}

fn fit_frame(frame: &mut Frame, target_rows: usize) {
    let target_rows = target_rows.max(1);
    if frame.lines.len() > target_rows {
        let dropped = frame.lines.len() - target_rows;
        frame.lines.drain(0..dropped);
        frame.cursor_line = frame.cursor_line.saturating_sub(dropped);
        frame.dock_index = frame.dock_index.saturating_sub(dropped);
        frame.composer_index = frame
            .composer_index
            .map(|index| index.saturating_sub(dropped));
        frame.activity_index = frame
            .activity_index
            .and_then(|index| index.checked_sub(dropped));
    } else if frame.lines.len() < target_rows {
        let padding = target_rows - frame.lines.len();
        let dock_index = frame.dock_index.min(frame.lines.len());
        frame.lines.splice(
            dock_index..dock_index,
            (0..padding).map(|_| PaintLine::blank()),
        );
        if frame.cursor_line >= dock_index {
            frame.cursor_line += padding;
        }
        if frame
            .composer_index
            .is_some_and(|index| index >= dock_index)
        {
            frame.composer_index = frame.composer_index.map(|index| index + padding);
        }
        if frame
            .activity_index
            .is_some_and(|index| index >= dock_index)
        {
            frame.activity_index = frame.activity_index.map(|index| index + padding);
        }
    }
    frame.cursor_line = frame.cursor_line.min(frame.lines.len().saturating_sub(1));
}

fn status_line_row(status: Option<StatusLineView>, fallback: &str, width: u16) -> PaintLine {
    let Some(status) = status else {
        return PaintLine {
            prefix: " ".to_owned(),
            prefix_tone: Tone::Muted,
            text: compact_right(fallback, width.saturating_sub(2) as usize),
            tone: Tone::Muted,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        };
    };

    let has_effort = status
        .effort
        .as_deref()
        .is_some_and(|effort| !effort.is_empty());
    let mut spans = Vec::new();
    let mut picks = Vec::new();
    if let Some(model) = status.model.filter(|model| !model.is_empty()) {
        let span = push_status_span(
            &mut spans,
            format!(" {} ", compact_right(&model, 28)),
            status_model_tone(&model).unwrap_or(Tone::StatusText),
        );
        picks.push((span, Pick::Model));
    }
    if let Some(effort) = status.effort.filter(|effort| !effort.is_empty()) {
        let tone = status_effort_tone(&effort).unwrap_or(Tone::StatusText);
        let span = push_status_span(&mut spans, format!("◆ {effort}"), tone);
        picks.push((span, Pick::EffortSetting));
    }
    if let Some(context) = status.context.filter(|context| !context.is_empty()) {
        // Keep the context marker so a narrow status row can preferentially
        // remove it before the compact reset reading. It stays aligned with the
        // ordinary status text colour.
        push_status_span(&mut spans, context, Tone::StatusText);
    }
    // The 5h window is dropped entirely when unknown rather than shown as a stub.
    let five_hour_remaining = status.five_hour_remaining.filter(|left| !left.is_empty());
    let five_hour = match (status.five_hour_percent, five_hour_remaining) {
        (Some(percent), Some(left)) => Some(format!("{left}: {percent}%")),
        (Some(percent), None) => Some(format!("5h: {percent}%")),
        (None, Some(left)) => Some(format!("5h: {left}")),
        (None, None) => None,
    };
    if let Some(five_hour) = five_hour {
        push_status_span(&mut spans, five_hour, Tone::StatusText);
    }
    if let Some(percent) = status.weekly_percent {
        push_status_span(&mut spans, format!("week: {percent}%"), Tone::StatusText);
    }
    // Fast: On/Off lives on the composer top rule beside the permission mode.
    if let Some(notice) = status.notice.filter(|notice| !notice.is_empty()) {
        push_status_span(&mut spans, notice, Tone::Muted);
    }
    // Align with the activity controls above by keeping two blank terminal
    // columns to the right of the status line.
    let max_width = width.saturating_sub(3) as usize;
    let shortcut_hint = status_line_shortcut_hint_from_effort(has_effort);
    let content_width = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum::<usize>();
    let hint_width = UnicodeWidthStr::width(shortcut_hint);
    if content_width + hint_width <= max_width {
        spans.push(PaintSpan {
            text: " ".repeat(max_width - content_width - hint_width),
            tone: Tone::Muted,
            bold: false,
        });
        spans.push(PaintSpan {
            text: shortcut_hint.to_owned(),
            tone: Tone::Muted,
            bold: false,
        });
    }
    trim_status_spans(&mut spans, max_width);

    let first = spans.first().cloned().unwrap_or(PaintSpan {
        text: String::new(),
        tone: Tone::Muted,
        bold: false,
    });
    PaintLine {
        prefix: " ".to_owned(),
        prefix_tone: Tone::Border,
        text: first.text,
        tone: first.tone,
        bold: first.bold,
        tool_heading: None,
        pick: None,
        tail: spans.into_iter().skip(1).collect(),
    }
    .with_picks(&picks)
}

fn status_line_shortcut_hint_from_effort(has_effort: bool) -> &'static str {
    if has_effort {
        "Shift + ↑↓ model · ←→ effort"
    } else {
        "Shift + ↑↓ model"
    }
}

fn side_panel_divider(content_width: usize) -> PaintLine {
    PaintLine {
        text: "─".repeat(content_width),
        tone: Tone::SidePanelDivider,
        ..PaintLine::plain("")
    }
}

fn side_panel_section_heading(
    title: &str,
    expanded: bool,
    content_width: usize,
    pick: Pick,
) -> PaintLine {
    let prefix = if expanded { "▲ " } else { "▼ " };
    let text = compact_right(
        title,
        content_width.saturating_sub(UnicodeWidthStr::width(prefix)),
    );
    let clickable_width = UnicodeWidthStr::width(prefix) + UnicodeWidthStr::width(text.as_str());
    PaintLine {
        prefix: prefix.to_owned(),
        prefix_tone: Tone::Muted,
        text,
        tone: Tone::Plain,
        bold: true,
        pick: Some(PickRegions::span(0, clickable_width, pick)),
        ..PaintLine::plain("")
    }
}

fn side_panel_status_lines(
    status: Option<&StatusLineView>,
    content_width: usize,
) -> Vec<PaintLine> {
    let Some(status) = status else {
        return Vec::new();
    };
    let Some(context) = status
        .context
        .as_deref()
        .filter(|context| !context.is_empty())
    else {
        return Vec::new();
    };
    let context_tone = status
        .model
        .as_deref()
        .and_then(model_tone)
        .unwrap_or(Tone::Border);
    vec![
        side_panel_divider(content_width),
        side_panel_context_line(context, content_width, context_tone),
    ]
}

fn move_context_to_side_panel(
    status: &mut Option<StatusLineView>,
    content_width: usize,
) -> Vec<PaintLine> {
    let lines = side_panel_status_lines(status.as_ref(), content_width);
    if !lines.is_empty()
        && let Some(status) = status.as_mut()
    {
        status.context = None;
    }
    lines
}

fn side_panel_context_line(context: &str, content_width: usize, context_tone: Tone) -> PaintLine {
    let value = context.strip_prefix("ctx: ").unwrap_or(context);
    let (counts, percent) = value
        .rsplit_once(" (")
        .and_then(|(counts, percent)| {
            percent
                .strip_suffix("%)")
                .and_then(|percent| percent.parse::<u8>().ok())
                .map(|percent| (counts, percent.min(100)))
        })
        .unwrap_or((value, 0));
    let counts = counts
        .split_once('/')
        .map(|(used, window)| {
            format!(
                "{}/{}K",
                used.trim_end_matches(['k', 'K']),
                window.trim_end_matches(['k', 'K'])
            )
        })
        .unwrap_or_else(|| counts.to_owned());
    let summary = format!("{counts} ({percent}%)");
    let label = "Context: ";
    let fixed_width = UnicodeWidthStr::width(label) + UnicodeWidthStr::width(summary.as_str()) + 3;
    let track = content_width
        .saturating_sub(fixed_width)
        .min(PROGRESS_TRACK_COLUMNS);
    if track == 0 {
        return PaintLine::plain(compact_right(&format!("{label}{summary}"), content_width));
    }

    let filled = ((track as f32 * percent as f32 / 100.0).round() as usize).min(track);
    PaintLine {
        text: label.to_owned(),
        bold: true,
        tail: vec![
            PaintSpan {
                text: "█".repeat(filled),
                tone: context_tone,
                bold: false,
            },
            PaintSpan {
                text: "░".repeat(track.saturating_sub(filled)),
                tone: Tone::Muted,
                bold: false,
            },
            PaintSpan {
                text: format!(" {summary}"),
                tone: Tone::StatusText,
                bold: false,
            },
        ],
        ..PaintLine::plain("")
    }
}

fn push_status_span(spans: &mut Vec<PaintSpan>, text: impl Into<String>, tone: Tone) -> usize {
    if spans.is_empty() {
        spans.push(PaintSpan {
            text: text.into(),
            tone,
            bold: false,
        });
        return 0;
    }
    spans.push(PaintSpan {
        text: " | ".to_owned(),
        tone: Tone::StatusSeparator,
        bold: false,
    });
    spans.push(PaintSpan {
        text: text.into(),
        tone,
        bold: false,
    });
    spans.len() - 1
}

fn trim_spans(spans: &mut Vec<PaintSpan>, max_width: usize) -> bool {
    let mut overflow = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum::<usize>()
        .saturating_sub(max_width);
    let trimmed = overflow > 0;
    while overflow > 0 {
        let Some(last) = spans.last_mut() else {
            break;
        };
        let Some(ch) = last.text.pop() else {
            spans.pop();
            continue;
        };
        overflow = overflow.saturating_sub(UnicodeWidthChar::width(ch).unwrap_or(0));
        if last.text.is_empty() {
            spans.pop();
        }
    }
    trimmed
}

const STATUS_TRUNCATION_MARKER: &str = "...";

fn trim_status_spans(spans: &mut Vec<PaintSpan>, max_width: usize) {
    let width = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum::<usize>();
    let mut trimmed = false;
    trimmed |= width > max_width;
    if trimmed {
        let marker_width = UnicodeWidthStr::width(STATUS_TRUNCATION_MARKER);
        trim_spans(spans, max_width.saturating_sub(marker_width));
        if max_width >= marker_width {
            spans.push(PaintSpan {
                text: STATUS_TRUNCATION_MARKER.to_owned(),
                tone: Tone::Muted,
                bold: false,
            });
        }
    }
}

/// Painted rows of tool output kept on screen. The renderer clips each output
/// box to five rows and hides the overflow, so counting
/// *painted* rows rather than lines of text is the point: one 900-character line
/// wraps into a screenful, and a line cap would happily let it through.
const TOOL_OUTPUT_ROWS: usize = 5;

/// Command and tool results: the heading, then at most [`TOOL_OUTPUT_ROWS`]
/// painted rows taken from the *end* of the output, and a muted `… +N lines`.
/// Output is printed verbatim rather than through the markdown pipeline — a
/// shell writes text, not documents.
fn tool_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let mut lines = wrapped_line("● ", Tone::Accent, &block.title, Tone::Plain, true, width);
    // Blank rows say nothing and would spend the row budget, so they go before
    // anything is counted. A shell almost always leaves at least one.
    let rows = block
        .body
        .lines()
        .filter(|row| !row.trim().is_empty())
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return lines;
    }
    // The tail, not the head: a command's verdict — its error, its summary, the
    // last thing it touched — lands at the end of what it printed.
    let tail = &rows[rows.len().saturating_sub(TOOL_OUTPUT_ROWS)..];
    let mut shown = 0;
    let mut painted = Vec::new();
    for row in tail {
        let wrapped = wrapped_line("  ", Tone::Muted, row, Tone::Muted, false, width);
        let room = TOOL_OUTPUT_ROWS - painted.len();
        if wrapped.len() > room {
            // A row too tall to finish is still worth a glimpse, but it stays
            // counted as hidden: most of it never reaches the screen.
            painted.extend(wrapped.into_iter().take(room));
            break;
        }
        painted.extend(wrapped);
        shown += 1;
    }
    lines.extend(painted);
    let hidden = rows.len() - shown;
    if hidden > 0 {
        lines.extend(wrapped_line(
            "  ",
            Tone::Muted,
            &format!("… +{hidden} lines"),
            Tone::Muted,
            false,
            width,
        ));
    }
    lines
}

fn is_bash_block(block: &Block) -> bool {
    matches!(block.kind, BlockKind::Tool | BlockKind::Warning) && block.title.starts_with("Shell ·")
}

fn is_running_shell_anchor(block: &Block) -> bool {
    let text = format!("{}\n{}", block.title, block.body).to_ascii_lowercase();
    text.contains("running") && text.contains("shell") && text.contains("command")
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

fn is_thinking_block(block: &Block) -> bool {
    matches!(block.kind, BlockKind::Reasoning) && block.title == THINKING_TITLE
}

fn is_empty_thinking_block(block: &Block) -> bool {
    is_thinking_block(block) && block.body.trim().is_empty()
}

fn is_file_change_block(block: &Block) -> bool {
    matches!(block.kind, BlockKind::FileChange)
}

/// Compaction is a transcript milestone, not a response boundary. In Shell Hide
/// it must not keep the stale pre-command thinking placeholder alive.
fn is_context_compaction_block(block: &Block) -> bool {
    matches!(block.kind, BlockKind::System) && block.title == "Context compacted"
}

/// Shell anchors are deliberately ordinary tool blocks while they run, so
/// Collapse can still show their progress. Hide filters them alongside completed
/// Shell groups before rendering; that also lets two thoughts separated only by
/// a hidden Shell collapse into their latest summary.
fn visible_transcript_blocks<'a>(
    blocks: impl IntoIterator<Item = &'a Block>,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
) -> Vec<&'a Block> {
    let mut visible: Vec<&Block> = Vec::new();
    for block in blocks {
        if is_empty_thinking_block(block) {
            continue;
        }
        if shell_display_mode == ShellDisplayMode::Hide
            && (is_bash_block(block)
                || is_running_shell_anchor(block)
                || is_web_search_block(block)
                || is_auxiliary_tool_block(block))
        {
            continue;
        }
        if diff_display_mode == DiffDisplayMode::Hide && is_file_change_block(block) {
            continue;
        }
        if is_thinking_block(block) {
            let prior_thinking = visible
                .iter()
                .rposition(|previous| is_thinking_block(previous));
            let adjacent_thinking = prior_thinking.is_some_and(|index| index + 1 == visible.len());
            let only_compaction_since = shell_display_mode == ShellDisplayMode::Hide
                && prior_thinking.is_some_and(|index| {
                    visible[index + 1..]
                        .iter()
                        .all(|previous| is_context_compaction_block(previous))
                });
            if adjacent_thinking || only_compaction_since {
                visible.remove(prior_thinking.expect("thinking index present"));
            }
        }
        visible.push(block);
    }
    visible
}

fn hidden_thinking_merge_at_history_boundary(
    history: &[Block],
    committed: &[Block],
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
) -> bool {
    if shell_display_mode != ShellDisplayMode::Hide {
        return false;
    }
    let before = visible_transcript_blocks(history, shell_display_mode, diff_display_mode);
    if before.is_empty() || committed.is_empty() {
        return false;
    }
    let mut combined = Vec::with_capacity(history.len() + committed.len());
    combined.extend_from_slice(history);
    combined.extend_from_slice(committed);
    let after = visible_transcript_blocks(&combined, shell_display_mode, diff_display_mode);

    // Inline output is permanent once printed. If filtering the just-completed
    // batch removes an older thought (including across a compaction card), repaint
    // the transcript instead of leaving that stale Thinking row on screen.
    before
        .iter()
        .any(|previous| !after.iter().any(|current| current.id() == previous.id()))
}

fn replaces_inline_history(history: &[Block], committed: &[Block]) -> bool {
    committed.iter().any(|incoming| {
        let child_ids = matches!(incoming.kind, BlockKind::ProgressGroup).then(|| {
            incoming
                .children()
                .iter()
                .map(Block::id)
                .collect::<HashSet<_>>()
        });
        history.iter().any(|existing| {
            existing.id() == incoming.id()
                || child_ids
                    .as_ref()
                    .is_some_and(|ids| ids.contains(&existing.id()))
        })
    })
}

fn bash_lines(block: &Block, width: u16, expanded: bool) -> Vec<PaintLine> {
    let title_tone = if matches!(block.kind, BlockKind::Warning) {
        Tone::Warning
    } else {
        Tone::Plain
    };
    if !expanded {
        let marker = "▸ ";
        let available = usize::from(width)
            .saturating_sub(UnicodeWidthStr::width(marker) + 1)
            .max(1);
        return vec![PaintLine {
            prefix: marker.to_owned(),
            prefix_tone: Tone::User,
            text: compact_right(&block.title, available),
            tone: title_tone,
            bold: true,
            tool_heading: Some(block.id()),
            pick: None,
            tail: Vec::new(),
        }];
    }

    let mut lines = wrapped_line("▾ ", Tone::User, &block.title, title_tone, true, width);
    for line in &mut lines {
        line.tool_heading = Some(block.id());
    }
    if !block.children().is_empty() {
        for child in block.children() {
            let child_tone = if matches!(child.kind, BlockKind::Warning) {
                Tone::Warning
            } else {
                Tone::Plain
            };
            lines.extend(wrapped_line(
                "  • ",
                Tone::Muted,
                &child.title,
                child_tone,
                true,
                width,
            ));
            for row in child.body.lines().filter(|row| !row.trim().is_empty()) {
                lines.extend(wrapped_line(
                    "    ",
                    Tone::Muted,
                    row,
                    Tone::Muted,
                    false,
                    width,
                ));
            }
        }
        return lines;
    }
    for row in block.body.lines().filter(|row| !row.trim().is_empty()) {
        lines.extend(wrapped_line(
            "  ",
            Tone::Muted,
            row,
            Tone::Muted,
            false,
            width,
        ));
    }
    lines
}

/// Rows for a Shell block under the global display setting. A direct click on a
/// visible heading always wins over the setting and reveals that block in full.
fn shell_group_lines(
    block: &Block,
    width: u16,
    shell_display_mode: ShellDisplayMode,
    expanded: bool,
) -> Vec<PaintLine> {
    match shell_display_mode {
        ShellDisplayMode::Hide => Vec::new(),
        ShellDisplayMode::Collapse => bash_lines(block, width, expanded),
        ShellDisplayMode::Expand if expanded => bash_lines(block, width, true),
        ShellDisplayMode::Expand => bash_preview_lines(block, width),
    }
}

/// The automatic Expand view is deliberately a preview: output spends one
/// five-row budget across every child command, while command headings remain
/// visible in their original order.
fn bash_preview_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    const OUTPUT_ROWS: usize = 5;

    let title_tone = if matches!(block.kind, BlockKind::Warning) {
        Tone::Warning
    } else {
        Tone::Plain
    };
    let mut lines = wrapped_line("▾ ", Tone::User, &block.title, title_tone, true, width);
    for line in &mut lines {
        line.tool_heading = Some(block.id());
    }

    let mut remaining = OUTPUT_ROWS;

    if block.children().is_empty() {
        append_bash_preview_output(&mut lines, "  ", &block.body, width, &mut remaining);
    } else {
        for child in block.children() {
            let child_tone = if matches!(child.kind, BlockKind::Warning) {
                Tone::Warning
            } else {
                Tone::Plain
            };
            lines.extend(wrapped_line(
                "  • ",
                Tone::Muted,
                &child.title,
                child_tone,
                true,
                width,
            ));
            append_bash_preview_output(&mut lines, "    ", &child.body, width, &mut remaining);
        }
    }
    lines
}

fn append_bash_preview_output(
    lines: &mut Vec<PaintLine>,
    prefix: &str,
    body: &str,
    width: u16,
    remaining: &mut usize,
) {
    for row in body.lines().filter(|row| !row.trim().is_empty()) {
        if *remaining == 0 {
            break;
        }
        let wrapped = wrapped_line(prefix, Tone::Muted, row, Tone::Muted, false, width);
        let shown = wrapped.len().min(*remaining);
        lines.extend(wrapped.into_iter().take(shown));
        *remaining -= shown;
    }
}

/// Diff rows a single `fileChange` block prints before it starts counting. Well
/// above what an ordinary edit produces, so the patch is normally shown whole,
/// but low enough that a sweeping refactor can't push the turn off screen.
const FILE_CHANGE_ROWS: usize = 40;

fn file_change_summary_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let mut lines = file_change_heading_lines(block, width);
    let (added, removed) = file_change_counts(block);
    lines.extend(wrapped_line(
        "  ⎿ ",
        Tone::Muted,
        &format!("Added {added} · Removed {removed}"),
        Tone::Muted,
        false,
        width,
    ));
    lines
}

fn file_change_heading_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let mut lines = wrapped_line("● ", Tone::Accent, &block.title, Tone::Plain, true, width);
    for line in &mut lines {
        line.tool_heading = Some(block.id());
    }
    lines
}

fn file_change_counts(block: &Block) -> (usize, usize) {
    if !block.children().is_empty() {
        return block
            .children()
            .iter()
            .map(file_change_counts)
            .fold((0, 0), |total, counts| {
                (total.0 + counts.0, total.1 + counts.1)
            });
    }
    block
        .body
        .lines()
        .skip(1)
        .fold((0usize, 0usize), |(added, removed), row| {
            if row.starts_with('+') && !row.starts_with("+++") {
                (added + 1, removed)
            } else if row.starts_with('-') && !row.starts_with("---") {
                (added, removed + 1)
            } else {
                (added, removed)
            }
        })
}

fn file_change_expanded_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    if block.children().is_empty() {
        return file_change_lines(block, width);
    }
    block
        .children()
        .iter()
        .flat_map(|child| file_change_expanded_lines(child, width))
        .map(|mut line| {
            if line.tool_heading.is_some() {
                line.tool_heading = Some(block.id());
            }
            line
        })
        .collect()
}

/// File edits use an `Update(path)` heading, line counts hanging off it under a
/// `⎿`, then the patch itself in a line-number
/// gutter. `+`/`-` rows carry the diff tint the whole way to the right edge, which
/// [`print_line`] paints from their tone. Hunk headers are consumed for their
/// numbers rather than printed — the gutter says the same thing somewhere more
/// useful.
fn file_change_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let mut lines = file_change_heading_lines(block, width);
    let mut rows = block.body.lines();
    if let Some(summary) = rows.next() {
        lines.extend(styled_lines(
            "  ⎿ ",
            Tone::Muted,
            counted_summary(summary),
            Tone::Muted,
            false,
            width,
        ));
    }

    let rows: Vec<&str> = rows.collect();
    let word_spans = intraline_highlights(&rows);

    let mut old_row = 0;
    let mut new_row = 0;
    let mut shown = 0;
    let mut hidden = 0;
    for (index, row) in rows.iter().copied().enumerate() {
        if let Some((old, new)) = hunk_start(row) {
            old_row = old;
            new_row = new;
            continue;
        }
        if shown == FILE_CHANGE_ROWS {
            hidden += 1;
            continue;
        }
        shown += 1;

        let (number, marker, tone, content) = match row.as_bytes().first() {
            Some(b'+') => {
                new_row += 1;
                (Some(new_row - 1), '+', Tone::DiffAdded, &row[1..])
            }
            Some(b'-') => {
                old_row += 1;
                (Some(old_row - 1), '-', Tone::DiffRemoved, &row[1..])
            }
            // A row a producer left unpadded is still context, just empty.
            Some(b' ') | None => {
                old_row += 1;
                new_row += 1;
                (
                    Some(new_row - 1),
                    ' ',
                    Tone::Plain,
                    row.get(1..).unwrap_or_default(),
                )
            }
            // Anything else is the per-file heading a multi-file batch adds.
            _ => (None, ' ', Tone::Plain, row),
        };

        lines.extend(match number {
            Some(number) => {
                let gutter = format!("{number:>8} {marker} ");
                // An unmarked row is context, so only its number dims.
                let gutter_tone = if marker == ' ' { Tone::Muted } else { tone };
                match word_spans[index].clone() {
                    Some(spans) => styled_lines(&gutter, gutter_tone, spans, tone, false, width),
                    None => wrapped_line(&gutter, gutter_tone, content, tone, false, width),
                }
            }
            None => wrapped_line("  ", Tone::Muted, content, Tone::Plain, true, width),
        });
    }

    if hidden > 0 {
        lines.extend(wrapped_line(
            "  ",
            Tone::Muted,
            &format!("… +{hidden} lines"),
            Tone::Muted,
            false,
            width,
        ));
    }
    lines
}

/// The word tints for every row of a patch, `None` for rows that only get their
/// full-width band.
///
/// A `-` run followed by a `+` run is a rewrite, so its rows pair off in order
/// and each pair is compared word by word — the same reading Claude Code shows.
/// Where the runs are different lengths the extra rows are a plain deletion or
/// insertion with nothing to compare against, and [`word_diff`] drops any pair
/// that turns out not to resemble its counterpart after all.
fn intraline_highlights(rows: &[&str]) -> Vec<Option<Vec<PaintSpan>>> {
    let mut spans = vec![None; rows.len()];
    let mut index = 0;
    while index < rows.len() {
        if !rows[index].starts_with('-') {
            index += 1;
            continue;
        }
        let removed = index;
        while index < rows.len() && rows[index].starts_with('-') {
            index += 1;
        }
        let added = index;
        while index < rows.len() && rows[index].starts_with('+') {
            index += 1;
        }
        for offset in 0..(added - removed).min(index - added) {
            let (old, new) = (removed + offset, added + offset);
            if let Some((old_spans, new_spans)) = word_diff(&rows[old][1..], &rows[new][1..]) {
                spans[old] = Some(old_spans);
                spans[new] = Some(new_spans);
            }
        }
    }
    spans
}

/// Words a line kept, past which comparing costs more than it explains. A rewrite
/// this long is read as a whole line anyway.
const WORD_DIFF_TOKENS: usize = 256;

/// Splits one changed line against its counterpart into the words they share and
/// the words only one of them has, as `(removed, added)` spans.
///
/// `None` means word tints would not help: the two lines have too little in
/// common to be a rewrite of each other, or nothing in common to leave untinted,
/// and either way the row's own band already says everything.
fn word_diff(old: &str, new: &str) -> Option<(Vec<PaintSpan>, Vec<PaintSpan>)> {
    let old_words = split_words(old);
    let new_words = split_words(new);
    if old_words.is_empty()
        || new_words.is_empty()
        || old_words.len() > WORD_DIFF_TOKENS
        || new_words.len() > WORD_DIFF_TOKENS
    {
        return None;
    }

    // `common[i][j]` is the longest run of shared words in `old[i..]`/`new[j..]`,
    // the ordinary longest-common-subsequence table. Walking it forwards from
    // `[0][0]` then reads off which words survived the edit.
    let mut common = vec![vec![0usize; new_words.len() + 1]; old_words.len() + 1];
    for i in (0..old_words.len()).rev() {
        for j in (0..new_words.len()).rev() {
            common[i][j] = if old_words[i] == new_words[j] {
                common[i + 1][j + 1] + 1
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }

    let mut removed = Vec::new();
    let mut added = Vec::new();
    let mut shared = 0;
    let (mut i, mut j) = (0, 0);
    while i < old_words.len() && j < new_words.len() {
        if old_words[i] == new_words[j] {
            push_highlight_span(&mut removed, old_words[i], Tone::DiffRemoved, false);
            push_highlight_span(&mut added, new_words[j], Tone::DiffAdded, false);
            if !old_words[i].trim().is_empty() {
                shared += 1;
            }
            i += 1;
            j += 1;
        } else if common[i + 1][j] >= common[i][j + 1] {
            push_highlight_span(&mut removed, old_words[i], Tone::DiffRemovedWord, false);
            i += 1;
        } else {
            push_highlight_span(&mut added, new_words[j], Tone::DiffAddedWord, false);
            j += 1;
        }
    }
    for word in &old_words[i..] {
        push_highlight_span(&mut removed, word, Tone::DiffRemovedWord, false);
    }
    for word in &new_words[j..] {
        push_highlight_span(&mut added, word, Tone::DiffAddedWord, false);
    }

    // Shared whitespace is not a resemblance — two unrelated lines still both
    // start indented. Below a quarter of the longer line the rows are read as
    // separate lines, not as one edited line, so they keep their plain band.
    let visible = |words: &[&str]| words.iter().filter(|word| !word.trim().is_empty()).count();
    if shared == 0 || shared * 4 < visible(&old_words).max(visible(&new_words)) {
        return None;
    }
    let tinted = |spans: &[PaintSpan]| {
        spans
            .iter()
            .any(|span| word_background(span.tone).is_some())
    };
    (tinted(&removed) || tinted(&added)).then_some((removed, added))
}

/// Splits a line into the units a word diff compares: runs of identifier
/// characters, runs of whitespace, and every other character on its own. Naming a
/// whole identifier as one unit is what keeps `old_name` → `new_name` tinting the
/// name rather than the letters it happens to share.
fn split_words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let ch = text[start..]
            .chars()
            .next()
            .expect("start is on a character boundary");
        let end = if is_word_char(ch) {
            take_while(text, start, is_word_char)
        } else if ch.is_whitespace() {
            take_while(text, start, char::is_whitespace)
        } else {
            start + ch.len_utf8()
        };
        words.push(&text[start..end]);
        start = end;
    }
    words
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// The counts are the part of a summary a reader scans for, so they come out of
/// its dim row brighter and bold while the words around them stay muted. Split on
/// digit runs rather than on the phrasing, so rewording the summary can't quietly
/// drop the emphasis.
fn counted_summary(summary: &str) -> Vec<PaintSpan> {
    let mut spans = Vec::new();
    let mut rest = summary;
    while !rest.is_empty() {
        let counting = rest.as_bytes()[0].is_ascii_digit();
        let end = rest
            .find(|ch: char| ch.is_ascii_digit() != counting)
            .unwrap_or(rest.len());
        let (run, tail) = rest.split_at(end);
        push_highlight_span(
            &mut spans,
            run,
            if counting { Tone::Plain } else { Tone::Muted },
            counting,
        );
        rest = tail;
    }
    spans
}

/// The starting old and new line numbers in a unified diff's `@@ -12,7 +12,9 @@`.
fn hunk_start(row: &str) -> Option<(usize, usize)> {
    let (ranges, _) = row.strip_prefix("@@ ")?.split_once(" @@")?;
    let (old, new) = ranges.split_once(' ')?;
    let start = |range: &str, sign: char| -> Option<usize> {
        range.strip_prefix(sign)?.split(',').next()?.parse().ok()
    };
    Some((start(old, '-')?, start(new, '+')?))
}

/// Title the app-server's reasoning summaries stream under, and the only one
/// that renders as a bare thought instead of a labelled section.
const THINKING_TITLE: &str = "Thinking…";
const UPDATED_PLAN_TITLE: &str = "Updated Plan";
const HISTORY_TITLE: &str = "History";

/// Codex paints a plan update as `- 작업 단계`, the explanation hanging off
/// a `└`, then one checkbox row per step indented four columns: `✔` for done,
/// `□` for the rest, with the in-progress step lit instead of dimmed. The body
/// carries `▸` for that step so the row keeps a status the text alone can't.
fn plan_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let title = if block.title == "작업 단계" {
        UPDATED_PLAN_TITLE
    } else {
        &block.title
    };
    let mut lines = wrapped_line("- ", Tone::Plain, title, Tone::Plain, true, width);
    let mut steps = 0usize;
    for row in block.body.lines().filter(|row| !row.trim().is_empty()) {
        // The checkbox itself is never struck through, only the step behind it.
        let (prefix, tone, bold, text) = if let Some(rest) = row.strip_prefix("└ ") {
            ("  └ ", Tone::Muted, false, rest)
        } else if let Some(rest) = row.strip_prefix("✔ ").or_else(|| row.strip_prefix("✓ ")) {
            steps += 1;
            ("    ✔ ", Tone::PlanDone, false, rest)
        } else if let Some(rest) = row.strip_prefix("▸ ") {
            steps += 1;
            ("    □ ", Tone::Accent, true, rest)
        } else if let Some(rest) = row.strip_prefix("□ ") {
            steps += 1;
            ("    □ ", Tone::Muted, false, rest)
        } else {
            ("    ", Tone::Muted, false, row)
        };
        lines.extend(wrapped_line(prefix, Tone::Muted, text, tone, bold, width));
    }
    if steps == 0 {
        lines.extend(wrapped_line(
            "    ",
            Tone::Muted,
            "(no steps provided)",
            Tone::Thinking,
            false,
            width,
        ));
    }
    lines.push(PaintLine::blank());
    lines
}

/// The key that folds the plan panel, on every runtime. Shift+Tab belongs to
/// Claude's permission modes, and a Korean IME eats Shift+Space, so the panel
/// names the one key that always works. Alt+P is reserved for the side panel.
const PLAN_TOGGLE_HINT: &str = " Alt + W ";

/// The tone the plan card's own box is drawn in. The docked side panel shares it
/// so the frame around the plan looks the same wherever the plan is shown.
const PLAN_BORDER_TONE: Tone = Tone::Plain;

/// Reasoning summaries use a narrow `∴` gutter and a single dim italic
/// paragraph. Plan blocks keep their heading and one physical row per step.
fn fixed_plan_summary_lines(
    summary: &PlanSummary,
    width: u16,
    phase: f32,
    plan_active: bool,
    plan_shimmer_phase: Option<f32>,
    plan_effort: Option<&str>,
) -> Vec<PaintLine> {
    let line_width = panel_span(width);
    let completed = summary
        .steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::Completed)
        .count();
    let effort_tone = plan_effort
        .and_then(effort_tone)
        .unwrap_or_else(|| plan_effort_tone(summary.steps.len()));
    let all_completed = !summary.steps.is_empty() && completed == summary.steps.len();
    let completion_displayed = all_completed && !plan_active;
    let displayed_completed = completed.saturating_sub(usize::from(all_completed && plan_active));
    let title = if all_completed && plan_active {
        format!(
            "{UPDATED_PLAN_TITLE} · {displayed_completed} / {} 진행 중",
            summary.steps.len()
        )
    } else {
        format!(
            "{UPDATED_PLAN_TITLE} · {completed} / {}",
            summary.steps.len()
        )
    };
    if !summary.expanded {
        let tail = format!("{PLAN_TOGGLE_HINT}▼ ──");
        let rule = "─".repeat(line_width.saturating_sub(
            5 + UnicodeWidthStr::width(title.as_str()) + UnicodeWidthStr::width(tail.as_str()),
        ));
        let header = PaintLine {
            prefix: String::new(),
            prefix_tone: Tone::Border,
            text: format!("─── {title} {rule}"),
            tone: PLAN_BORDER_TONE,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: vec![
                PaintSpan {
                    text: PLAN_TOGGLE_HINT.to_owned(),
                    tone: Tone::FastOff,
                    bold: false,
                },
                PaintSpan {
                    text: "▼ ".to_owned(),
                    tone: PLAN_BORDER_TONE,
                    bold: false,
                },
                PaintSpan {
                    text: "──".to_owned(),
                    tone: PLAN_BORDER_TONE,
                    bold: false,
                },
            ],
        };
        return vec![
            header.with_picks(&[(1, Pick::PlanSummary), (2, Pick::PlanSummary)]),
            PaintLine::blank(),
        ];
    }
    let mut lines = Vec::new();
    let steps = summary.steps.iter().collect::<Vec<_>>();
    let last_step_index = steps.len().saturating_sub(1);
    for (index, step) in steps.into_iter().enumerate() {
        let response_cleanup_step = plan_active && all_completed && index == last_step_index;
        let (prefix, bold) = match step.status {
            PlanStepStatus::Completed if response_cleanup_step => (
                format!(
                    "  {}  ",
                    WORKING_SPINNER
                        [(phase.clamp(0.0, 0.999) * WORKING_SPINNER.len() as f32) as usize]
                ),
                true,
            ),
            PlanStepStatus::Completed => ("  ✔  ".to_owned(), false),
            PlanStepStatus::InProgress if plan_active => (
                format!(
                    "  {}  ",
                    WORKING_SPINNER
                        [(phase.clamp(0.0, 0.999) * WORKING_SPINNER.len() as f32) as usize]
                ),
                true,
            ),
            PlanStepStatus::InProgress => ("  ▸  ".to_owned(), false),
            PlanStepStatus::Pending => ("     ".to_owned(), false),
        };
        let elapsed_text = step.elapsed.map(format_plan_elapsed);
        let elapsed = elapsed_text.as_deref();
        let time_width = elapsed
            .map(|time| UnicodeWidthStr::width(time) + 3)
            .unwrap_or_default();
        let task_width = line_width
            .saturating_sub(UnicodeWidthStr::width(prefix.as_str()))
            .saturating_sub(time_width);
        let task_text = compact_right(&step.text, task_width);
        let is_completed = step.status == PlanStepStatus::Completed && !response_cleanup_step;
        let in_progress = step.status == PlanStepStatus::InProgress || response_cleanup_step;
        lines.push(PaintLine {
            prefix,
            prefix_tone: if is_completed {
                Tone::FastOff
            } else if in_progress {
                Tone::Accent
            } else {
                Tone::Muted
            },
            text: task_text,
            tone: if is_completed {
                Tone::PlanDone
            } else if in_progress {
                Tone::Accent
            } else {
                Tone::Plain
            },
            bold,
            tool_heading: None,
            pick: None,
            tail: elapsed
                .map(|time| {
                    vec![PaintSpan {
                        text: format!(" ({time})"),
                        tone: Tone::Muted,
                        bold: false,
                    }]
                })
                .unwrap_or_default(),
        });
    }
    let header_tail = format!("{PLAN_TOGGLE_HINT}▲ ─┐");
    let header_rule = "─".repeat(line_width.saturating_sub(
        5 + UnicodeWidthStr::width(title.as_str()) + UnicodeWidthStr::width(header_tail.as_str()),
    ));
    let mut header_tail = vec![PaintSpan {
        text: "┌── ".to_owned(),
        tone: PLAN_BORDER_TONE,
        bold: false,
    }];
    header_tail.extend(plan_title_shimmer_spans(
        &title,
        plan_shimmer_phase,
        effort_tone,
    ));
    header_tail.extend([
        PaintSpan {
            text: format!(" {header_rule}"),
            tone: PLAN_BORDER_TONE,
            bold: false,
        },
        PaintSpan {
            text: PLAN_TOGGLE_HINT.to_owned(),
            tone: Tone::FastOff,
            bold: false,
        },
        PaintSpan {
            text: "▲ ".to_owned(),
            tone: PLAN_BORDER_TONE,
            bold: false,
        },
        PaintSpan {
            text: "─┐".to_owned(),
            tone: PLAN_BORDER_TONE,
            bold: false,
        },
    ]);
    let header = PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: String::new(),
        tone: Tone::Plain,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: header_tail,
    };
    let header = header.with_picks(&[(4, Pick::PlanSummary), (5, Pick::PlanSummary)]);
    lines.insert(0, header);
    lines.insert(1, PaintLine::blank());
    if completion_displayed {
        let elapsed = summary.steps.iter().filter_map(|step| step.elapsed).sum();
        let total = format!("⏱  {}", format_plan_elapsed(elapsed));
        lines.push(PaintLine::plain(format!(
            "{}{}  ",
            " ".repeat(line_width.saturating_sub(UnicodeWidthStr::width(total.as_str()) + 2)),
            total
        )));
    } else {
        lines.push(PaintLine::blank());
    }
    lines.push(PaintLine {
        tone: PLAN_BORDER_TONE,
        ..PaintLine::plain(format!("└{}┘", "─".repeat(line_width.saturating_sub(2))))
    });
    lines.push(PaintLine::blank());
    lines
}

/// A new plan starts at low twice, then advances one effort colour per added step.
fn plan_effort_tone(step_count: usize) -> Tone {
    match step_count.saturating_sub(2) {
        0 => Tone::EffortLow,
        1 => Tone::EffortMedium,
        2 => Tone::EffortHigh,
        3 => Tone::EffortXHigh,
        4 => Tone::EffortMax,
        _ => Tone::EffortUltra,
    }
}

fn plan_title_shimmer_spans(text: &str, phase: Option<f32>, effort_tone: Tone) -> Vec<PaintSpan> {
    let Some(phase) = phase else {
        return vec![PaintSpan {
            text: text.to_owned(),
            tone: Tone::Plain,
            bold: false,
        }];
    };
    let loop_phase = (phase.clamp(0.0, 0.999) * PLAN_SHIMMER_LOOPS) % 1.0;
    shimmer_spans_with_band(
        text,
        loop_phase,
        theme::palette().foreground,
        PLAN_SHIMMER_BAND,
    )
    .into_iter()
    .map(|span| PaintSpan {
        tone: match span.tone {
            Tone::Shimmer(_, level) => Tone::PlanShimmer(
                tone_rgb(effort_tone).unwrap_or(theme::palette().foreground),
                level,
            ),
            tone => tone,
        },
        ..span
    })
    .collect()
}

/// The plan summary as the docked panel shows it: a heading, a blank row, one
/// row per step, another blank row, then a quiet section rule without card
/// chrome or a toggle hint.
fn side_panel_plan_lines(
    summary: &PlanSummary,
    content_width: usize,
    phase: f32,
    plan_active: bool,
) -> Vec<PaintLine> {
    if content_width == 0 {
        return Vec::new();
    }
    let completed = summary
        .steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::Completed)
        .count();
    let all_completed = !summary.steps.is_empty() && completed == summary.steps.len();
    let completion_displayed = all_completed && !plan_active;
    let displayed_completed = completed.saturating_sub(usize::from(all_completed && plan_active));
    let mut title = if all_completed && plan_active {
        format!(
            "{UPDATED_PLAN_TITLE}  {displayed_completed} / {} 진행 중",
            summary.steps.len()
        )
    } else {
        format!(
            "{UPDATED_PLAN_TITLE}  {completed} / {}",
            summary.steps.len()
        )
    };
    // The card prints its own total once every step is done. The panel has no
    // room for a line of its own, so the same total rides on the heading.
    if completion_displayed {
        let elapsed: Duration = summary.steps.iter().filter_map(|step| step.elapsed).sum();
        title.push_str(&format!("  [⏱  {}]", format_plan_elapsed(elapsed)));
    }
    let heading =
        side_panel_section_heading(&title, summary.expanded, content_width, Pick::PlanSummary);
    if !summary.expanded {
        return vec![
            heading,
            PaintLine::blank(),
            side_panel_divider(content_width),
        ];
    }
    let mut lines = vec![heading, PaintLine::blank()];
    let last_step_index = summary.steps.len().saturating_sub(1);
    for (index, step) in summary.steps.iter().enumerate() {
        let elapsed_text = step.elapsed.map(format_plan_elapsed);
        let elapsed = elapsed_text.as_deref();
        let time_width = elapsed
            .map(|time| UnicodeWidthStr::width(time) + 3)
            .unwrap_or_default();
        let response_cleanup_step = plan_active && all_completed && index == last_step_index;
        let is_completed = step.status == PlanStepStatus::Completed && !response_cleanup_step;
        let in_progress = step.status == PlanStepStatus::InProgress || response_cleanup_step;
        // The status mark keeps its own gutter so every step's text starts on the
        // same column, whether or not the step carries a mark.
        let (mark, bold) = match step.status {
            PlanStepStatus::Completed if response_cleanup_step => (
                format!(
                    "{} ",
                    WORKING_SPINNER
                        [(phase.clamp(0.0, 0.999) * WORKING_SPINNER.len() as f32) as usize]
                ),
                true,
            ),
            PlanStepStatus::Completed => ("✔ ".to_owned(), false),
            PlanStepStatus::InProgress if plan_active => (
                format!(
                    "{} ",
                    WORKING_SPINNER
                        [(phase.clamp(0.0, 0.999) * WORKING_SPINNER.len() as f32) as usize]
                ),
                true,
            ),
            PlanStepStatus::InProgress => ("▸ ".to_owned(), false),
            PlanStepStatus::Pending => ("  ".to_owned(), false),
        };
        let mark_width = UnicodeWidthStr::width(mark.as_str());
        let prefix_tone = if is_completed {
            Tone::FastOff
        } else if in_progress {
            Tone::Accent
        } else {
            Tone::Muted
        };
        let tone = if is_completed {
            Tone::PlanDone
        } else if in_progress {
            Tone::Accent
        } else {
            Tone::Plain
        };
        // A step keeps one row of its own: too wide for the panel and it ends in
        // an ellipsis rather than pushing the steps below it down.
        lines.push(PaintLine {
            prefix: mark,
            prefix_tone,
            bold,
            text: compact_right(
                &step.text,
                content_width.saturating_sub(time_width + mark_width),
            ),
            tone,
            tail: elapsed
                .map(|time| {
                    vec![PaintSpan {
                        text: format!(" ({time})"),
                        tone: Tone::Muted,
                        bold: false,
                    }]
                })
                .unwrap_or_default(),
            ..PaintLine::plain("")
        });
    }
    if !summary.steps.is_empty() {
        lines.push(PaintLine::blank());
    }
    lines.push(side_panel_divider(content_width));
    lines
}

const SIDE_PANEL_PROMPT_LIMIT: usize = 5;

/// The latest sent prompts as a compact panel section: heading, blank row,
/// newest-first entries, another blank row, then a quiet section rule.
fn side_panel_prompt_lines(
    history: &[Block],
    content_width: usize,
    expanded: bool,
) -> Vec<PaintLine> {
    if content_width == 0 {
        return Vec::new();
    }
    let heading =
        side_panel_section_heading("Input Prompt", expanded, content_width, Pick::PromptSection);
    if !expanded {
        return vec![
            heading,
            PaintLine::blank(),
            side_panel_divider(content_width),
        ];
    }
    let mut lines = vec![heading, PaintLine::blank()];
    let mut has_prompts = false;
    for prompt in history
        .iter()
        .rev()
        .filter(|block| matches!(block.kind, BlockKind::User))
        .take(SIDE_PANEL_PROMPT_LIMIT)
    {
        has_prompts = true;
        let text = prompt.body.split_whitespace().collect::<Vec<_>>().join(" ");
        let marker_tone = model_tone(&prompt.title).unwrap_or(Tone::User);
        let prefix = "› ";
        let available = content_width.saturating_sub(UnicodeWidthStr::width(prefix));
        lines.push(PaintLine {
            prefix: prefix.to_owned(),
            prefix_tone: marker_tone,
            text: compact_right(&text, available),
            tone: Tone::Plain,
            pick: Some(PickRegions::span(
                0,
                content_width,
                Pick::Prompt(prompt.id()),
            )),
            ..PaintLine::plain("")
        });
    }
    if has_prompts {
        lines.push(PaintLine::blank());
    }
    lines.push(side_panel_divider(content_width));
    lines
}

/// Provider integration snapshots use the same compact, borderless hierarchy as
/// the reference panel. The selected provider is always first because short
/// terminals may not have room for both snapshots.
fn side_panel_integration_lines(
    providers: &[ProviderIntegrationView],
    content_width: usize,
    max_rows: usize,
) -> Vec<PaintLine> {
    if content_width == 0 || max_rows == 0 {
        return Vec::new();
    }
    let mut providers = providers.iter().collect::<Vec<_>>();
    providers.sort_by_key(|provider| !provider.active);

    let mut lines = Vec::new();
    for (index, provider) in providers.into_iter().enumerate() {
        if index > 0 {
            lines.push(PaintLine::blank());
        }
        lines.push(side_panel_provider_heading(provider, content_width));
        if !provider.enabled {
            continue;
        }
        lines.extend(side_panel_integration_section(
            "MCP",
            provider.mcp.as_deref(),
            provider.mcp_error.as_deref(),
            provider.mcp_expanded,
            Pick::McpSection(provider.provider.clone()),
            content_width,
        ));
        lines.extend(side_panel_integration_section(
            "Plugin",
            provider.plugins.as_deref(),
            provider.plugin_error.as_deref(),
            provider.plugins_expanded,
            Pick::PluginSection(provider.provider.clone()),
            content_width,
        ));
    }

    if lines.len() <= max_rows {
        return lines;
    }
    if max_rows == 1 {
        lines.truncate(1);
        return lines;
    }
    let hidden = lines.len().saturating_sub(max_rows - 1);
    lines.truncate(max_rows - 1);
    while lines
        .last()
        .is_some_and(|line| painted_line_width(line) == 0)
    {
        lines.pop();
    }
    lines.push(PaintLine {
        tone: Tone::Muted,
        ..PaintLine::plain(compact_right(&format!("… +{hidden}"), content_width))
    });
    lines
}

fn side_panel_provider_heading(
    provider: &ProviderIntegrationView,
    content_width: usize,
) -> PaintLine {
    let (prefix, prefix_tone, detail) = if !provider.enabled {
        ("× ", Tone::Error, "연결 안 됨")
    } else if provider.active {
        ("▸ ", Tone::Accent, "사용 중")
    } else if provider.mcp.is_some() || provider.plugins.is_some() {
        ("· ", Tone::Muted, "최근 확인")
    } else {
        ("· ", Tone::Muted, "대기")
    };
    let available = content_width.saturating_sub(UnicodeWidthStr::width(prefix));
    PaintLine {
        prefix: prefix.to_owned(),
        prefix_tone,
        text: compact_right(&format!("{}  {detail}", provider.provider), available),
        tone: Tone::Plain,
        bold: true,
        ..PaintLine::plain("")
    }
}

fn side_panel_integration_section(
    title: &str,
    items: Option<&[IntegrationItemView]>,
    error: Option<&str>,
    expanded: bool,
    pick: Pick,
    content_width: usize,
) -> Vec<PaintLine> {
    let heading = side_panel_section_heading(title, expanded, content_width, pick);
    if !expanded {
        return vec![heading];
    }
    let mut lines = vec![heading];
    match items {
        Some(items) if !items.is_empty() => {
            lines.extend(
                items
                    .iter()
                    .map(|item| side_panel_integration_item_line(item, content_width)),
            );
            if error.is_some() {
                lines.push(side_panel_integration_placeholder(
                    "새로고침 실패",
                    content_width,
                ));
            }
        }
        Some(_) if error.is_some() => lines.push(side_panel_integration_placeholder(
            "현재 상태 미확인",
            content_width,
        )),
        Some(_) => lines.push(side_panel_integration_placeholder("없음", content_width)),
        None => lines.push(side_panel_integration_placeholder(
            if error.is_some() {
                "현재 상태 미확인"
            } else {
                "아직 확인하지 않음"
            },
            content_width,
        )),
    }
    lines
}

fn side_panel_integration_item_line(item: &IntegrationItemView, content_width: usize) -> PaintLine {
    let (prefix, prefix_tone) = match item.state {
        IntegrationItemState::Active => ("● ", Tone::Success),
        IntegrationItemState::Inactive => ("× ", Tone::Error),
        IntegrationItemState::Pending => ("○ ", Tone::Warning),
        IntegrationItemState::Unknown => ("· ", Tone::Muted),
    };
    let available = content_width.saturating_sub(UnicodeWidthStr::width(prefix));
    let label = if item.detail.is_empty() {
        item.name.clone()
    } else {
        format!("{}  {}", item.name, item.detail)
    };
    PaintLine {
        prefix: prefix.to_owned(),
        prefix_tone,
        text: compact_right(&label, available),
        tone: Tone::Plain,
        ..PaintLine::plain("")
    }
}

fn side_panel_integration_placeholder(text: &str, content_width: usize) -> PaintLine {
    PaintLine {
        prefix: "· ".to_owned(),
        prefix_tone: Tone::Muted,
        text: compact_right(text, content_width.saturating_sub(2)),
        tone: Tone::Muted,
        ..PaintLine::plain("")
    }
}

fn format_plan_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds / 60 {
        0 => format!("{seconds}s"),
        minutes => format!("{minutes}m {}s", seconds % 60),
    }
}

fn reasoning_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    // `/plan` output shares this block kind but keeps its heading.
    let titled = block.title != THINKING_TITLE;
    let mut lines = if titled {
        wrapped_line("✻ ", Tone::Muted, &block.title, Tone::Plain, true, width)
    } else {
        Vec::new()
    };
    if block.body.is_empty() {
        if !titled {
            // Nothing has streamed yet, so the label stands in for the thought.
            lines.extend(wrapped_line(
                "✻ ",
                Tone::Muted,
                THINKING_TITLE,
                Tone::Thinking,
                false,
                width,
            ));
        }
        return lines;
    }
    if titled {
        for row in block.body.lines() {
            if row.is_empty() {
                lines.push(PaintLine::blank());
            } else {
                lines.extend(wrapped_line(
                    "  ",
                    Tone::Muted,
                    row,
                    Tone::Thinking,
                    false,
                    width,
                ));
            }
        }
    } else {
        let body = block.body.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.extend(wrapped_line(
            "∴ ",
            Tone::Thinking,
            &body,
            Tone::Thinking,
            false,
            width,
        ));
    }
    lines
}

#[cfg(test)]
fn block_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    CHAT_LAYOUT.store(true, Ordering::Relaxed);
    block_lines_with_expansion(block, width, false)
}

fn progress_group_lines(
    block: &Block,
    width: u16,
    expanded: bool,
    reveal: Option<f32>,
) -> Vec<PaintLine> {
    let showing_body = expanded || reveal.is_some_and(|value| value > f32::EPSILON);
    let marker = if showing_body { "▾ " } else { "▸ " };
    let mut lines = vec![PaintLine {
        prefix: marker.to_owned(),
        prefix_tone: Tone::FastOff,
        text: block.title.clone(),
        tone: Tone::Muted,
        bold: false,
        tool_heading: Some(block.id()),
        pick: None,
        tail: Vec::new(),
    }];
    if !showing_body {
        return lines;
    }
    lines.push(PaintLine::blank());
    lines.extend(progress_group_body_lines(block, width, reveal));
    lines
}

fn embedded_progress_group_lines(
    block: &Block,
    width: u16,
    expanded: bool,
    reveal: Option<f32>,
) -> Vec<PaintLine> {
    if !expanded && reveal.is_none_or(|value| value <= f32::EPSILON) {
        return Vec::new();
    }
    let mut lines = progress_group_body_lines(block, width, reveal);
    if !lines.is_empty() {
        lines.push(PaintLine::blank());
    }
    lines
}

fn progress_group_body_lines(block: &Block, width: u16, reveal: Option<f32>) -> Vec<PaintLine> {
    let mut body = block
        .children()
        .iter()
        .flat_map(|child| {
            block_lines_with_mode(
                child,
                width,
                ShellDisplayMode::Collapse,
                DiffDisplayMode::Expand,
                false,
            )
        })
        .collect::<Vec<_>>();
    while matches!(body.last(), Some(line) if line == &PaintLine::blank()) {
        body.pop();
    }
    if let Some(reveal) = reveal {
        let reveal = reveal.clamp(0.0, 1.0);
        let scaled = reveal * body.len() as f32;
        let visible = scaled.ceil() as usize;
        let start = body.len().saturating_sub(visible);
        body = body.split_off(start);
        if let Some(edge) = body.first_mut() {
            let fraction = scaled - scaled.floor();
            let opacity = if fraction <= f32::EPSILON {
                u8::MAX
            } else {
                (fraction.max(0.12) * 255.0).round() as u8
            };
            fade_response_line(edge, opacity);
        }
    }
    body
}

fn fade_response_line(line: &mut PaintLine, opacity: u8) {
    let fade = |tone| {
        Tone::ResponseTransition(
            tone_rgb(tone).unwrap_or_else(|| theme::palette().foreground),
            opacity,
        )
    };
    line.prefix_tone = fade(line.prefix_tone);
    line.tone = fade(line.tone);
    for span in &mut line.tail {
        span.tone = fade(span.tone);
    }
}

fn block_group_lines(
    block: &Block,
    width: u16,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    expanded: bool,
) -> Vec<PaintLine> {
    block_group_lines_at(
        block,
        width,
        shell_display_mode,
        diff_display_mode,
        expanded,
        None,
    )
}

fn block_group_lines_at(
    block: &Block,
    width: u16,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    expanded: bool,
    response_reveal: Option<f32>,
) -> Vec<PaintLine> {
    let mut lines = block_lines_with_mode_at(
        block,
        width,
        shell_display_mode,
        diff_display_mode,
        expanded,
        response_reveal,
    );
    while matches!(lines.last(), Some(line) if line == &PaintLine::blank()) {
        lines.pop();
    }
    if !lines.is_empty() {
        lines.push(PaintLine::blank());
    }
    lines
}

/// Claude opens a Korean sentence with an English connective — `Now 브리지에 …` —
/// and no wording of the language rule has stopped it: the habit belongs to the
/// English system prompt the preamble is generated under, not to the rule. So the
/// leak is repaired where it is shown. Only a bare connective directly in front
/// of Hangul is cut; a backtick-quoted mention or an all-English sentence is left
/// alone, since cutting those would change what the answer says.
fn without_leading_english_filler(body: &str) -> &str {
    const FILLERS: [&str; 16] = [
        "Now", "Next", "First", "Then", "Also", "So", "Finally", "Actually", "Okay", "OK",
        "Alright", "Fine", "Good", "Let me", "I'll", "Alt",
    ];

    for filler in FILLERS {
        let Some(rest) = body.strip_prefix(filler) else {
            continue;
        };
        let rest = rest
            .strip_prefix(|c| matches!(c, ',' | '.' | ':' | ';'))
            .unwrap_or(rest);
        // A streamed answer arrives a few characters at a time, so the opener
        // lands before the Hangul that proves it is one. Holding the row empty
        // for those few frames is what keeps the word off the screen entirely.
        if rest.is_empty() || rest == " " {
            return "";
        }
        let Some(rest) = rest.strip_prefix(' ') else {
            continue;
        };
        if rest.starts_with(is_hangul) {
            return rest;
        }
    }
    body
}

fn is_hangul(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3131}'..='\u{318E}')
}

fn block_lines_with_mode(
    block: &Block,
    width: u16,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    expanded: bool,
) -> Vec<PaintLine> {
    block_lines_with_mode_at(
        block,
        width,
        shell_display_mode,
        diff_display_mode,
        expanded,
        None,
    )
}

fn block_lines_with_mode_at(
    block: &Block,
    width: u16,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    expanded: bool,
    response_reveal: Option<f32>,
) -> Vec<PaintLine> {
    if is_bash_block(block) {
        return shell_group_lines(block, width, shell_display_mode, expanded);
    }
    if matches!(block.kind, BlockKind::Welcome) {
        let mut values = block.body.lines();
        let mut lines = welcome_lines(
            WelcomeView {
                provider: values.next().unwrap_or("Codex").to_owned(),
                plan: values.next().unwrap_or_default().to_owned(),
                cwd: values.next().unwrap_or_default().to_owned(),
                account: values.next().unwrap_or_default().to_owned(),
                credits: values.map(ToOwned::to_owned).collect(),
            },
            width,
        );
        lines.push(PaintLine::blank());
        return lines;
    }
    if matches!(block.kind, BlockKind::Update) {
        return update_lines(block, width);
    }
    if matches!(block.kind, BlockKind::User) {
        return user_prompt_lines(block, width);
    }
    if matches!(block.kind, BlockKind::ProgressGroup) {
        return progress_group_lines(block, width, expanded, response_reveal);
    }
    if matches!(block.kind, BlockKind::Reasoning) {
        return reasoning_lines(block, width);
    }
    if matches!(block.kind, BlockKind::Plan) {
        return plan_lines(block, width);
    }
    if matches!(block.kind, BlockKind::Tool) {
        return tool_lines(block, width);
    }
    if matches!(block.kind, BlockKind::FileChange) {
        return match diff_display_mode {
            DiffDisplayMode::Hide => Vec::new(),
            DiffDisplayMode::Collapse if expanded => file_change_expanded_lines(block, width),
            DiffDisplayMode::Collapse => file_change_summary_lines(block, width),
            DiffDisplayMode::Expand if expanded => file_change_summary_lines(block, width),
            DiffDisplayMode::Expand => file_change_expanded_lines(block, width),
        };
    }
    if matches!(block.kind, BlockKind::ModelChange) {
        let mut lines = vec![PaintLine {
            prefix: "  ".to_owned(),
            prefix_tone: Tone::ModelChange,
            text: block.title.clone(),
            tone: Tone::ModelChange,
            bold: true,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        }];
        // A title that says everything (Fast mode On) carries no detail line, so
        // an empty body drops the row instead of painting a hole under it.
        if !block.body.is_empty() {
            lines.push(PaintLine {
                prefix: "    ".to_owned(),
                prefix_tone: Tone::ModelChange,
                text: block.body.clone(),
                tone: Tone::ModelChange,
                bold: false,
                tool_heading: None,
                pick: None,
                tail: Vec::new(),
            });
        }
        lines.push(PaintLine::blank());
        return lines;
    }
    if is_context_compaction_block(block) {
        return wrapped_line("● ", Tone::Accent, &block.title, Tone::Accent, true, width);
    }

    let (marker, tone) = match block.kind {
        BlockKind::Welcome | BlockKind::Update | BlockKind::ModelChange => {
            unreachable!("handled above")
        }
        BlockKind::User => unreachable!("user blocks are rendered separately"),
        BlockKind::ProgressGroup
        | BlockKind::Reasoning
        | BlockKind::Plan
        | BlockKind::Tool
        | BlockKind::FileChange => {
            unreachable!("handled above")
        }
        BlockKind::Assistant => (RESPONSE_BULLET_PREFIX, Tone::FastOff),
        BlockKind::Diff => ("● ", Tone::Accent),
        BlockKind::Warning => ("▲ ", Tone::Warning),
        BlockKind::Error => ("✕ ", Tone::Error),
        BlockKind::System => ("◆ ", Tone::Muted),
    };

    let conversational = matches!(block.kind, BlockKind::Assistant);
    let chat_layout = conversational && CHAT_LAYOUT.load(Ordering::Relaxed);
    let marker = if chat_layout { "  " } else { marker };
    let conversational_width = if chat_layout {
        conversation_region_width(width)
            .saturating_add(1)
            .saturating_sub(CHAT_BUBBLE_RIGHT_GAP) as u16
    } else {
        width
    };
    let mut first_content = conversational;
    let content_tone = if chat_layout {
        Tone::AssistantBubble
    } else {
        Tone::Plain
    };
    let mut lines = if conversational {
        Vec::new()
    } else {
        wrapped_line(
            marker,
            tone,
            &block.title,
            Tone::Plain,
            true,
            conversational_width,
        )
    };
    if block.body.is_empty() {
        if conversational {
            lines.extend(wrapped_line(
                marker,
                tone,
                "",
                content_tone,
                false,
                conversational_width,
            ));
        }
        let lines = if chat_layout {
            assistant_chat_bubble_lines(lines)
        } else {
            lines
        };
        return lines;
    }

    let force_diff = matches!(block.kind, BlockKind::Diff);
    let mut code = false;
    let mut code_language = String::new();
    let body = if conversational {
        without_leading_english_filler(&block.body)
    } else {
        &block.body
    };
    let raw_lines = body.lines().collect::<Vec<_>>();
    let mut line_index = 0;
    while let Some(raw_line) = raw_lines.get(line_index).copied() {
        let trimmed = raw_line.trim_start();
        if let Some(language) = trimmed.strip_prefix("```") {
            if code {
                code_language.clear();
            } else {
                code_language = language.trim().to_ascii_lowercase();
            }
            code = !code;
            line_index += 1;
            continue;
        }

        if code {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  ", Tone::Muted);
            lines.extend(styled_lines(
                &prefix,
                prefix_tone,
                highlight_code(raw_line, &code_language),
                Tone::Code,
                false,
                conversational_width,
            ));
        } else if force_diff {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  ", Tone::Muted);
            lines.extend(diff_line(
                &prefix,
                prefix_tone,
                raw_line,
                conversational_width,
            ));
        } else if let Some(separator) = raw_lines.get(line_index + 1)
            && let Some(header) = markdown_table_cells(raw_line)
            && let Some(alignments) = markdown_table_alignments(separator, header.len())
        {
            let table_start = line_index;
            let mut rows = vec![header];
            line_index += 2;
            while let Some(row) = raw_lines
                .get(line_index)
                .and_then(|line| markdown_table_cells(line))
            {
                if row.len() != rows[0].len() {
                    break;
                }
                rows.push(row);
                line_index += 1;
            }
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  ", Tone::Muted);
            if let Some(table) = markdown_table_lines(
                &prefix,
                prefix_tone,
                &rows,
                &alignments,
                content_tone,
                conversational_width,
            ) {
                lines.extend(table);
                continue;
            }
            lines.extend(markdown_line(
                &prefix,
                prefix_tone,
                raw_line,
                content_tone,
                false,
                conversational_width,
            ));
            line_index = table_start;
        } else if trimmed.starts_with('#') {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  ", Tone::Muted);
            lines.extend(markdown_line(
                &prefix,
                prefix_tone,
                trimmed.trim_start_matches('#').trim_start(),
                Tone::MarkdownHeading,
                true,
                conversational_width,
            ));
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let continuation_prefix = if conversational { "  " } else { "  - " };
            let (prefix, prefix_tone) = body_prefix(
                &mut first_content,
                marker,
                tone,
                continuation_prefix,
                Tone::Plain,
            );
            lines.extend(markdown_line(
                &prefix,
                prefix_tone,
                item,
                content_tone,
                false,
                conversational_width,
            ));
        } else if let Some(quote) = trimmed.strip_prefix("> ") {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  │ ", Tone::Muted);
            lines.extend(wrapped_line(
                &prefix,
                prefix_tone,
                quote,
                Tone::Muted,
                false,
                conversational_width,
            ));
        } else {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  ", Tone::Muted);
            lines.extend(markdown_line(
                &prefix,
                prefix_tone,
                raw_line,
                content_tone,
                false,
                conversational_width,
            ));
        }
        line_index += 1;
    }

    if chat_layout {
        assistant_chat_bubble_lines(lines)
    } else {
        lines.push(PaintLine::blank());
        lines
    }
}

fn markdown_table_cells(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    if !line.contains('|') {
        return None;
    }
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = line.strip_suffix('|').unwrap_or(line);
    let cells = line
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect::<Vec<_>>();
    (cells.len() >= 2 && cells.iter().all(|cell| !cell.is_empty())).then_some(cells)
}

#[derive(Clone, Copy)]
enum TableAlignment {
    Left,
    Center,
    Right,
}

fn markdown_table_alignments(line: &str, columns: usize) -> Option<Vec<TableAlignment>> {
    let cells = markdown_table_cells(line)?;
    (cells.len() == columns).then_some(())?;
    cells
        .into_iter()
        .map(|cell| {
            let rule = cell.trim();
            let body = rule.trim_matches(':');
            (body.len() >= 3 && body.bytes().all(|byte| byte == b'-')).then(|| {
                match (rule.starts_with(':'), rule.ends_with(':')) {
                    (true, true) => TableAlignment::Center,
                    (false, true) => TableAlignment::Right,
                    _ => TableAlignment::Left,
                }
            })
        })
        .collect()
}

fn markdown_table_lines(
    prefix: &str,
    prefix_tone: Tone,
    rows: &[Vec<String>],
    alignments: &[TableAlignment],
    tone: Tone,
    width: u16,
) -> Option<Vec<PaintLine>> {
    let columns = rows.first()?.len();
    if alignments.len() != columns {
        return None;
    }
    let available = usize::from(width).saturating_sub(UnicodeWidthStr::width(prefix) + 1);
    const COLUMN_GAP: usize = 2;
    const MIN_CELL_WIDTH: usize = 4;
    let gap_width = columns.saturating_sub(1).saturating_mul(COLUMN_GAP);
    if available < gap_width.saturating_add(columns.saturating_mul(MIN_CELL_WIDTH)) {
        return None;
    }

    let mut widths = (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| UnicodeWidthStr::width(row[column].as_str()))
                .max()
                .unwrap_or(0)
                .max(MIN_CELL_WIDTH)
        })
        .collect::<Vec<_>>();
    let content_width = available.saturating_sub(gap_width);
    while widths.iter().sum::<usize>() > content_width {
        let (index, _) = widths
            .iter()
            .enumerate()
            .filter(|(_, value)| **value > MIN_CELL_WIDTH)
            .max_by_key(|(_, value)| **value)?;
        widths[index] -= 1;
    }

    let mut output = Vec::new();
    let continuation_prefix = " ".repeat(UnicodeWidthStr::width(prefix));
    for (row_index, row) in rows.iter().enumerate() {
        let cell_lines = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| table_cell_lines(cell, *width))
            .collect::<Vec<_>>();
        let height = cell_lines.iter().map(Vec::len).max().unwrap_or(1);
        for cell_line in 0..height {
            let cells = cell_lines
                .iter()
                .zip(&widths)
                .zip(alignments)
                .map(|((lines, width), alignment)| {
                    align_table_cell(
                        lines.get(cell_line).map(String::as_str).unwrap_or(""),
                        *width,
                        *alignment,
                    )
                })
                .collect::<Vec<_>>();
            let row_prefix = if output.is_empty() {
                prefix
            } else {
                &continuation_prefix
            };
            output.push(table_content_line(
                row_prefix,
                prefix_tone,
                &cells,
                if row_index == 0 {
                    Tone::MarkdownHeading
                } else {
                    tone
                },
                false,
                COLUMN_GAP,
            ));
        }
        if row_index + 1 < rows.len() {
            output.push(table_rule_line(
                &continuation_prefix,
                prefix_tone,
                &widths,
                COLUMN_GAP,
            ));
        }
    }
    Some(output)
}

fn table_content_line(
    prefix: &str,
    prefix_tone: Tone,
    cells: &[String],
    tone: Tone,
    bold: bool,
    column_gap: usize,
) -> PaintLine {
    let mut cells = cells.iter();
    let first = cells.next().cloned().unwrap_or_default();
    let mut tail = Vec::new();
    for cell in cells {
        tail.push(PaintSpan {
            text: " ".repeat(column_gap),
            tone,
            bold: false,
        });
        tail.push(PaintSpan {
            text: cell.clone(),
            tone,
            bold,
        });
    }
    PaintLine {
        prefix: prefix.to_owned(),
        prefix_tone,
        text: first,
        tone,
        bold,
        tool_heading: None,
        pick: None,
        tail,
    }
}

fn table_rule_line(
    prefix: &str,
    prefix_tone: Tone,
    widths: &[usize],
    column_gap: usize,
) -> PaintLine {
    let cells = widths
        .iter()
        .map(|width| "─".repeat(*width))
        .collect::<Vec<_>>();
    table_content_line(prefix, prefix_tone, &cells, Tone::Muted, false, column_gap)
}

fn table_cell_lines(cell: &str, width: usize) -> Vec<String> {
    textwrap::wrap(
        cell,
        textwrap::Options::new(width)
            .break_words(true)
            .word_separator(textwrap::WordSeparator::AsciiSpace),
    )
    .into_iter()
    .map(|line| line.into_owned())
    .collect::<Vec<_>>()
}

fn align_table_cell(cell: &str, width: usize, alignment: TableAlignment) -> String {
    let content_width = UnicodeWidthStr::width(cell);
    let padding = width.saturating_sub(content_width);
    let (left, right) = match alignment {
        TableAlignment::Left => (0, padding),
        TableAlignment::Center => (padding / 2, padding.saturating_sub(padding / 2)),
        TableAlignment::Right => (padding, 0),
    };
    format!("{}{}{}", " ".repeat(left), cell, " ".repeat(right))
}

fn assistant_chat_bubble_lines(mut lines: Vec<PaintLine>) -> Vec<PaintLine> {
    let width = lines
        .iter()
        .map(painted_line_width)
        .max()
        .unwrap_or(1)
        .saturating_add(CHAT_BUBBLE_PADDING + CHAT_BUBBLE_RIGHT_GAP);
    for line in &mut lines {
        let padding = width.saturating_sub(painted_line_width(line));
        if padding > 0 {
            line.tail.push(PaintSpan {
                text: " ".repeat(padding),
                tone: Tone::AssistantBubble,
                bold: false,
            });
        }
        // Keep the bubble band on syntax and inline-highlight spans too. The
        // empty span is only a row marker; it emits no terminal cells.
        line.tail.push(PaintSpan {
            text: String::new(),
            tone: Tone::AssistantBubble,
            bold: false,
        });
    }
    let half = |glyph: char| PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Plain,
        text: glyph.to_string().repeat(width),
        tone: Tone::AssistantBubbleHalf,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: Vec::new(),
    };
    let mut bubble = vec![half('▄')];
    bubble.append(&mut lines);
    bubble.push(half('▀'));
    bubble.push(PaintLine::blank());
    bubble
}

#[cfg(test)]
fn block_lines_with_expansion(block: &Block, width: u16, expanded: bool) -> Vec<PaintLine> {
    block_lines_with_mode(
        block,
        width,
        ShellDisplayMode::Collapse,
        DiffDisplayMode::Expand,
        expanded,
    )
}

fn user_prompt_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    user_prompt_lines_with_history(block, width, None, CHAT_LAYOUT.load(Ordering::Relaxed))
}

fn user_prompt_lines_with_history(
    block: &Block,
    width: u16,
    history: Option<(u64, &str, bool)>,
    chat_layout: bool,
) -> Vec<PaintLine> {
    let marker_tone = model_tone(&block.title).unwrap_or(Tone::User);
    if !chat_layout {
        // 세로선은 블록에 기록된 전송 시점 모델 색을 쓰고, 모델을 못 알아보면 기존 강조색으로 돌아간다.
        let border_tone = model_tone(&block.title).unwrap_or(Tone::Accent);
        let lines = block
            .body
            .lines()
            .flat_map(|line| {
                wrapped_line_with_continuation(
                    "▌ ",
                    "▌ ",
                    border_tone,
                    line,
                    Tone::UserPrompt,
                    false,
                    width.saturating_sub(1),
                )
            })
            .collect::<Vec<_>>();
        // 본문 줄의 배경은 마지막 한 칸을 비워 두고 칠해진다. 여백 줄의 공백도
        // 같은 칸에서 끝나야 프롬프트의 오른쪽 끝이 한 줄로 맞는다.
        let padding_width = usize::from(width).saturating_sub(3);
        let mut top = PaintLine::user_prompt_padding(padding_width);
        top.prefix = "▌ ".to_owned();
        top.prefix_tone = border_tone;
        let bottom = top.clone();
        let mut lines = lines;
        lines.insert(0, top);
        lines.push(bottom);
        attach_history_to_prompt(&mut lines, width, history, false);
        if history.is_some() {
            lines.push(PaintLine::blank());
        }
        return lines;
    }
    const RIGHT_GAP: usize = 0;

    let region_width = conversation_region_width(width);
    let left_margin = usize::from(width)
        .saturating_sub(1)
        .saturating_sub(region_width);
    // The `> ` marker sits inside the region too, so its two columns come off
    // the content along with the padding on both sides of the bubble.
    let content_width = region_width
        .saturating_sub(RIGHT_GAP + (CHAT_BUBBLE_PADDING + CHAT_BUBBLE_RIGHT_GAP) * 2 + 2);
    let raw_lines = if block.body.is_empty() {
        vec![""]
    } else {
        block.body.lines().collect()
    };
    let mut lines = raw_lines
        .into_iter()
        .flat_map(|raw_line| {
            wrapped_line(
                "",
                Tone::Plain,
                raw_line,
                Tone::UserPrompt,
                false,
                content_width.saturating_add(1) as u16,
            )
        })
        .collect::<Vec<_>>();
    for line in &mut lines {
        line.text = format!(
            "{}{}",
            line.text,
            " ".repeat(CHAT_BUBBLE_PADDING + CHAT_BUBBLE_RIGHT_GAP)
        );
    }
    let text_width = lines
        .iter()
        .map(|line| UnicodeWidthStr::width(line.text.as_str()))
        .max()
        .unwrap_or(CHAT_BUBBLE_PADDING * 2);
    let bubble_width = text_width + CHAT_BUBBLE_PADDING + CHAT_BUBBLE_RIGHT_GAP + 2;
    let half_prefix =
        " ".repeat(left_margin + region_width.saturating_sub(RIGHT_GAP + bubble_width));
    for (index, line) in lines.iter_mut().enumerate() {
        let padding = text_width.saturating_sub(UnicodeWidthStr::width(line.text.as_str()));
        if padding > 0 {
            line.text.push_str(&" ".repeat(padding));
        }
        line.prefix = format!(
            "{}{}{}",
            half_prefix,
            " ".repeat(CHAT_BUBBLE_PADDING + CHAT_BUBBLE_RIGHT_GAP),
            if index == 0 { "› " } else { "  " },
        );
        line.prefix_tone = marker_tone;
    }
    let mut top = PaintLine::user_prompt_padding(bubble_width);
    top.prefix = half_prefix.clone();
    let mut bottom = PaintLine::user_prompt_padding(bubble_width);
    bottom.prefix = half_prefix;
    lines.insert(0, top);
    lines.push(bottom);
    attach_history_to_prompt(&mut lines, width, history, true);
    if history.is_some() {
        lines.push(PaintLine::blank());
    }
    lines
}

fn attach_history_to_prompt(
    lines: &mut [PaintLine],
    width: u16,
    history: Option<(u64, &str, bool)>,
    chat_layout: bool,
) {
    let Some((group_id, title, _expanded)) = history else {
        return;
    };
    let protected_right = usize::from(width).saturating_sub(1);
    for line in lines.iter_mut() {
        let prefix_width = UnicodeWidthStr::width(line.prefix.as_str());
        let start = if chat_layout {
            prefix_width
        } else {
            // Keep the coloured model border itself unchanged, but include the
            // padding cell immediately after it in the prompt-wide hover.
            prefix_width.saturating_sub(1)
        };
        let end = if chat_layout {
            painted_line_width(line).min(protected_right)
        } else {
            protected_right
        };
        if start < end {
            line.pick = Some(PickRegions::span(start, end, Pick::History(group_id)));
        }
    }

    let label = title.to_owned();
    let label_width = UnicodeWidthStr::width(label.as_str());
    let Some(bottom) = lines.last_mut() else {
        return;
    };
    let padding_width = UnicodeWidthStr::width(bottom.text.as_str());
    if label_width > padding_width {
        return;
    }
    let right_padding = padding_width.saturating_sub(label_width).min(2);
    // Replace the suffix of the existing padding instead of growing the row:
    // the terminal's protected autowrap cell therefore remains untouched.
    bottom.text = " ".repeat(padding_width - label_width - right_padding);
    bottom.tail.clear();
    bottom.tail.push(PaintSpan {
        text: label,
        tone: Tone::History,
        bold: false,
    });
    bottom.tail.push(PaintSpan {
        text: " ".repeat(right_padding),
        tone: Tone::UserPromptPadding,
        bold: false,
    });
}

fn conversation_region_width(width: u16) -> usize {
    usize::from(width).saturating_sub(1).saturating_mul(80) / 100
}

fn body_prefix(
    first_content: &mut bool,
    marker: &str,
    marker_tone: Tone,
    default: &str,
    default_tone: Tone,
) -> (String, Tone) {
    if *first_content {
        *first_content = false;
        (marker.to_owned(), marker_tone)
    } else {
        (default.to_owned(), default_tone)
    }
}

fn markdown_line(
    prefix: &str,
    prefix_tone: Tone,
    text: &str,
    tone: Tone,
    bold: bool,
    width: u16,
) -> Vec<PaintLine> {
    if !text.contains('`') && !text.contains("**") && !text.contains('[') {
        return wrapped_line(prefix, prefix_tone, text, tone, bold, width);
    }

    let mut spans = Vec::new();
    let mut links = Vec::new();
    let mut index = 0;
    let mut strong = bold;
    while index < text.len() {
        let rest = &text[index..];
        if rest.starts_with("**") {
            strong = !strong;
            index += 2;
            continue;
        }
        if let Some(after_tick) = rest.strip_prefix('`')
            && let Some(end) = after_tick.find('`')
        {
            push_highlight_span(&mut spans, &after_tick[..end], Tone::InlineCode, false);
            index += end + 2;
            continue;
        }
        if let Some((label, target, consumed)) = inline_link(rest) {
            push_highlight_span(&mut spans, &label, Tone::MarkdownLink, strong);
            links.push((label, target));
            index += consumed;
            continue;
        }

        let next_marker = [
            rest.find("**").unwrap_or(rest.len()),
            rest.find('`').unwrap_or(rest.len()),
            rest.find('[').unwrap_or(rest.len()),
        ]
        .into_iter()
        .min()
        .unwrap_or(rest.len());
        let take = if next_marker == 0 {
            rest.chars().next().map(char::len_utf8).unwrap_or(0)
        } else {
            next_marker
        };
        push_highlight_span(&mut spans, &rest[..take], tone, strong);
        index += take;
    }

    let mut lines = styled_lines(prefix, prefix_tone, spans, tone, bold, width);
    attach_markdown_link_picks(&mut lines, &links);
    lines
}

/// Collapses `[label](url)` — and the `![alt](url)` image form — down to the
/// label so a model's file links stop leaking raw markdown into the transcript.
/// A `:line` (or `:line:column`) tail on the target is worth reading, so it gets
/// grafted onto the label; the rest of the path is noise the label already says.
/// Returns the text to paint plus how many bytes of `rest` it consumed.
fn inline_link(rest: &str) -> Option<(String, String, usize)> {
    let image = rest.starts_with("![");
    let body = if image { &rest[1..] } else { rest };
    let after_bracket = body.strip_prefix('[')?;
    let close = after_bracket.find("](")?;
    let label = &after_bracket[..close];
    if label.contains('[') {
        return None;
    }
    let after_paren = &after_bracket[close + 2..];
    let end = after_paren.find(')')?;
    let raw_target = &after_paren[..end];
    let target = markdown_link_target_body(raw_target);
    let consumed = usize::from(image) + 1 + close + 2 + end + 1;

    let mut text = label.to_owned();
    if text.is_empty() {
        text = target.to_owned();
    } else if let Some(suffix) = line_suffix(target)
        && !text.ends_with(&suffix)
    {
        text.push_str(&suffix);
    }
    Some((text, markdown_link_open_target(raw_target), consumed))
}

/// Converts Codex-style local file targets into values understood by the
/// platform opener. Windows absolute paths arrive with a URI-like leading
/// slash, while source links can carry a line/column suffix that is useful in
/// the label but is not part of the file name.
fn markdown_link_open_target(target: &str) -> String {
    let target = markdown_link_target_body(target);
    let target = target
        .strip_prefix('/')
        .filter(|inner| is_windows_drive_path(inner))
        .unwrap_or(target);

    if is_local_file_target(target)
        && let Some(suffix) = line_suffix(target)
    {
        return target.strip_suffix(&suffix).unwrap_or(target).to_owned();
    }
    target.to_owned()
}

fn markdown_link_target_body(target: &str) -> &str {
    target
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
        .unwrap_or(target)
}

fn is_windows_drive_path(target: &str) -> bool {
    let bytes = target.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn is_local_file_target(target: &str) -> bool {
    is_windows_drive_path(target)
        || target.starts_with('/')
        || target.starts_with('\\')
        || target
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
}

/// Restores the link targets after styled wrapping. Markdown rendering keeps only
/// visible spans, so clickable regions are rebuilt from their display text in
/// parse order and attached to every wrapped fragment of the label.
fn attach_markdown_link_picks(lines: &mut [PaintLine], links: &[(String, String)]) {
    let mut links = links.iter();
    let Some((first_label, first_target)) = links.next() else {
        return;
    };
    let mut label = first_label.as_str();
    let mut target = first_target.as_str();
    let mut remaining = label.chars().count();

    for line in lines {
        let mut column = UnicodeWidthStr::width(line.prefix.as_str());
        let mut regions = Vec::new();
        for span in std::iter::once((&line.text, line.tone))
            .chain(line.tail.iter().map(|span| (&span.text, span.tone)))
        {
            for ch in span.0.chars() {
                let width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if span.1 == Tone::MarkdownLink && remaining > 0 {
                    regions.push((column, column + width, Pick::OpenLink(target.to_owned())));
                    remaining -= 1;
                    if remaining == 0 {
                        let Some((next_label, next_target)) = links.next() else {
                            column += width;
                            continue;
                        };
                        label = next_label.as_str();
                        target = next_target.as_str();
                        remaining = label.chars().count();
                    }
                }
                column += width;
            }
        }
        if !regions.is_empty() {
            line.pick = Some(PickRegions(regions));
        }
    }
}

/// The `:83` / `:83:12` tail of a file target, or `None` when the trailing
/// colon group isn't a position — a drive letter or a URL port is not a line.
fn line_suffix(target: &str) -> Option<String> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return None;
    }
    let is_number = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    let (head, tail) = target.rsplit_once(':')?;
    if !is_number(tail) || head.len() <= 1 {
        return None;
    }
    match head.rsplit_once(':') {
        Some((_, middle)) if is_number(middle) => Some(format!(":{middle}:{tail}")),
        _ => Some(format!(":{tail}")),
    }
}

fn styled_lines(
    prefix: &str,
    prefix_tone: Tone,
    spans: Vec<PaintSpan>,
    fallback_tone: Tone,
    fallback_bold: bool,
    width: u16,
) -> Vec<PaintLine> {
    let prefix_width = UnicodeWidthStr::width(prefix);
    let available = (width as usize).saturating_sub(prefix_width + 1).max(1);
    let mut rows: Vec<Vec<PaintSpan>> = vec![Vec::new()];
    let mut used = 0;
    let mut tokens: Vec<(bool, Vec<PaintSpan>, usize)> = Vec::new();

    for span in spans {
        for ch in span.text.chars() {
            let whitespace = ch.is_whitespace();
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if let Some((last_whitespace, parts, token_width)) = tokens.last_mut()
                && *last_whitespace == whitespace
            {
                push_highlight_span(parts, &ch.to_string(), span.tone, span.bold);
                *token_width += char_width;
            } else {
                tokens.push((
                    whitespace,
                    vec![PaintSpan {
                        text: ch.to_string(),
                        tone: span.tone,
                        bold: span.bold,
                    }],
                    char_width,
                ));
            }
        }
    }

    let mut pending_space = Vec::new();
    let mut pending_space_width = 0;
    for (whitespace, parts, token_width) in tokens {
        if whitespace {
            pending_space.extend(parts);
            pending_space_width += token_width;
            continue;
        }

        if used > 0 && used + pending_space_width + token_width > available {
            rows.push(Vec::new());
            used = 0;
            pending_space.clear();
            pending_space_width = 0;
        }
        if used > 0 {
            for span in pending_space.drain(..) {
                push_highlight_span(
                    rows.last_mut().expect("at least one styled row"),
                    &span.text,
                    span.tone,
                    span.bold,
                );
            }
            used += pending_space_width;
        }
        pending_space_width = 0;

        for span in parts {
            for ch in span.text.chars() {
                let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if used + char_width > available && used > 0 {
                    rows.push(Vec::new());
                    used = 0;
                }
                push_highlight_span(
                    rows.last_mut().expect("at least one styled row"),
                    &ch.to_string(),
                    span.tone,
                    span.bold,
                );
                used += char_width;
            }
        }
    }

    if rows.len() == 1 && rows[0].is_empty() {
        rows[0].push(PaintSpan {
            text: String::new(),
            tone: fallback_tone,
            bold: fallback_bold,
        });
    }

    let row_count = rows.len();
    rows.into_iter()
        .enumerate()
        .map(|(index, mut row)| {
            let first = row.remove(0);
            if index + 1 < row_count {
                row.push(PaintSpan {
                    text: String::new(),
                    tone: Tone::CopyJoin,
                    bold: false,
                });
            }
            PaintLine {
                prefix: if index == 0 {
                    prefix.to_owned()
                } else {
                    " ".repeat(prefix_width)
                },
                prefix_tone,
                text: first.text,
                tone: first.tone,
                bold: first.bold,
                tool_heading: None,
                pick: None,
                tail: row,
            }
        })
        .collect()
}

fn diff_line(prefix: &str, prefix_tone: Tone, text: &str, width: u16) -> Vec<PaintLine> {
    let tone = if text.starts_with("+++") || text.starts_with("---") {
        Tone::DiffHeader
    } else if text.starts_with('+') {
        Tone::DiffAdded
    } else if text.starts_with('-') {
        Tone::DiffRemoved
    } else if text.starts_with("@@")
        || text.starts_with("diff ")
        || text.starts_with("index ")
        || text.starts_with("new file ")
        || text.starts_with("deleted file ")
        || text.starts_with("rename ")
    {
        Tone::DiffHeader
    } else {
        Tone::Code
    };
    wrapped_line(prefix, prefix_tone, text, tone, false, width)
}

fn highlight_code(text: &str, language: &str) -> Vec<PaintSpan> {
    if let Some(spans) = syntax::highlight(text, language) {
        return spans
            .into_iter()
            .map(|span| PaintSpan {
                text: span.text,
                tone: match span.kind {
                    SyntaxKind::Plain => Tone::Code,
                    SyntaxKind::Comment => Tone::SyntaxComment,
                    SyntaxKind::String => Tone::SyntaxString,
                    SyntaxKind::Keyword => Tone::SyntaxKeyword,
                    SyntaxKind::Number => Tone::SyntaxNumber,
                    SyntaxKind::Type => Tone::SyntaxType,
                    SyntaxKind::Function => Tone::SyntaxFunction,
                    SyntaxKind::Attribute => Tone::SyntaxAttribute,
                    SyntaxKind::Property => Tone::SyntaxProperty,
                },
                bold: matches!(span.kind, SyntaxKind::Keyword | SyntaxKind::Type),
            })
            .collect();
    }

    let mut spans = Vec::new();
    let mut index = 0;
    let hash_comment = matches!(
        language,
        "py" | "python"
            | "rb"
            | "ruby"
            | "sh"
            | "bash"
            | "zsh"
            | "yaml"
            | "yml"
            | "toml"
            | "ps1"
            | "powershell"
    );
    let sql_comment = matches!(language, "sql");

    while index < text.len() {
        let rest = &text[index..];
        let starts_hash_comment = hash_comment
            && rest.starts_with('#')
            && (index == 0
                || text[..index]
                    .chars()
                    .next_back()
                    .is_some_and(|ch| ch.is_whitespace()));
        if rest.starts_with("//") || (sql_comment && rest.starts_with("--")) || starts_hash_comment
        {
            push_highlight_span(&mut spans, rest, Tone::SyntaxComment, false);
            break;
        }
        if rest.starts_with("/*") || rest.starts_with("<!--") {
            let terminator = if rest.starts_with("/*") { "*/" } else { "-->" };
            let end = rest[2..]
                .find(terminator)
                .map(|offset| index + 2 + offset + terminator.len())
                .unwrap_or(text.len());
            push_highlight_span(&mut spans, &text[index..end], Tone::SyntaxComment, false);
            index = end;
            continue;
        }

        let ch = rest
            .chars()
            .next()
            .expect("index is on a character boundary");
        if matches!(ch, '"' | '\'' | '`') {
            if ch == '\''
                && language == "rust"
                && rest[1..].chars().next().is_some_and(is_identifier_start)
                && !rest[1..].contains('\'')
            {
                push_highlight_span(&mut spans, "'", Tone::Code, false);
                index += 1;
                continue;
            }
            let mut end = index + ch.len_utf8();
            let mut escaped = false;
            while end < text.len() {
                let next = text[end..]
                    .chars()
                    .next()
                    .expect("end is on a character boundary");
                end += next.len_utf8();
                if next == ch && !escaped {
                    break;
                }
                escaped = next == '\\' && !escaped;
                if next != '\\' {
                    escaped = false;
                }
            }
            push_highlight_span(&mut spans, &text[index..end], Tone::SyntaxString, false);
            index = end;
            continue;
        }

        if ch.is_ascii_digit() {
            let end = take_while(text, index, |candidate| {
                candidate.is_ascii_alphanumeric() || matches!(candidate, '.' | '_' | 'x' | 'X')
            });
            push_highlight_span(&mut spans, &text[index..end], Tone::SyntaxNumber, false);
            index = end;
            continue;
        }

        if is_identifier_start(ch) {
            let end = take_while(text, index, is_identifier_continue);
            let identifier = &text[index..end];
            let next = text[end..]
                .chars()
                .find(|candidate| !candidate.is_whitespace());
            let (tone, bold) = if is_literal_constant(identifier) {
                (Tone::SyntaxNumber, false)
            } else if is_keyword(identifier) {
                (Tone::SyntaxKeyword, true)
            } else if identifier
                .chars()
                .next()
                .is_some_and(|first| first.is_uppercase())
            {
                (Tone::SyntaxType, true)
            } else if next == Some('(') {
                (Tone::SyntaxFunction, false)
            } else {
                (Tone::Code, false)
            };
            push_highlight_span(&mut spans, identifier, tone, bold);
            index = end;
            continue;
        }

        let end = index + ch.len_utf8();
        push_highlight_span(&mut spans, &text[index..end], Tone::Code, false);
        index = end;
    }

    spans
}

fn take_while(text: &str, start: usize, predicate: impl Fn(char) -> bool) -> usize {
    let mut end = start;
    for ch in text[start..].chars() {
        if !predicate(ch) {
            break;
        }
        end += ch.len_utf8();
    }
    end
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_alphabetic()
}

fn is_identifier_continue(ch: char) -> bool {
    is_identifier_start(ch) || ch.is_ascii_digit()
}

fn is_keyword(identifier: &str) -> bool {
    let identifier = identifier.to_ascii_lowercase();
    matches!(
        identifier.as_str(),
        "abstract"
            | "alter"
            | "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "base"
            | "bool"
            | "boolean"
            | "break"
            | "by"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "checked"
            | "class"
            | "const"
            | "continue"
            | "crate"
            | "create"
            | "decimal"
            | "def"
            | "delegate"
            | "delete"
            | "do"
            | "double"
            | "drop"
            | "dynamic"
            | "else"
            | "enum"
            | "event"
            | "export"
            | "extends"
            | "extern"
            | "final"
            | "finally"
            | "float"
            | "fn"
            | "for"
            | "foreach"
            | "from"
            | "fun"
            | "function"
            | "global"
            | "group"
            | "having"
            | "if"
            | "impl"
            | "import"
            | "in"
            | "inner"
            | "insert"
            | "int"
            | "integer"
            | "interface"
            | "internal"
            | "into"
            | "is"
            | "join"
            | "left"
            | "let"
            | "limit"
            | "lock"
            | "long"
            | "match"
            | "mod"
            | "mut"
            | "nameof"
            | "namespace"
            | "new"
            | "not"
            | "object"
            | "on"
            | "operator"
            | "or"
            | "order"
            | "out"
            | "outer"
            | "override"
            | "package"
            | "partial"
            | "private"
            | "protected"
            | "pub"
            | "public"
            | "readonly"
            | "record"
            | "ref"
            | "required"
            | "return"
            | "right"
            | "sbyte"
            | "sealed"
            | "select"
            | "self"
            | "set"
            | "short"
            | "sizeof"
            | "stackalloc"
            | "static"
            | "string"
            | "struct"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "trait"
            | "try"
            | "type"
            | "typeof"
            | "uint"
            | "ulong"
            | "unchecked"
            | "unsafe"
            | "update"
            | "use"
            | "ushort"
            | "using"
            | "var"
            | "values"
            | "virtual"
            | "void"
            | "volatile"
            | "where"
            | "while"
            | "with"
            | "yield"
    )
}

fn is_literal_constant(identifier: &str) -> bool {
    matches!(
        identifier.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "none" | "nil"
    )
}

fn push_highlight_span(spans: &mut Vec<PaintSpan>, text: &str, tone: Tone, bold: bool) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.tone == tone
        && last.bold == bold
    {
        last.text.push_str(text);
        return;
    }
    spans.push(PaintSpan {
        text: text.to_owned(),
        tone,
        bold,
    });
}

fn wrapped_line(
    prefix: &str,
    prefix_tone: Tone,
    text: &str,
    tone: Tone,
    bold: bool,
    width: u16,
) -> Vec<PaintLine> {
    let continuation = " ".repeat(UnicodeWidthStr::width(prefix));
    wrapped_line_with_continuation(prefix, &continuation, prefix_tone, text, tone, bold, width)
}

/// Like [`wrapped_line`], but the caller chooses what folded rows are prefixed
/// with — bordered panels need to repeat their `│` instead of blanking it out.
fn wrapped_line_with_continuation(
    prefix: &str,
    continuation: &str,
    prefix_tone: Tone,
    text: &str,
    tone: Tone,
    bold: bool,
    width: u16,
) -> Vec<PaintLine> {
    let width = width as usize;
    let prefix_width = UnicodeWidthStr::width(prefix).max(UnicodeWidthStr::width(continuation));
    // `CellFrame` leaves the physical final column blank to avoid terminal
    // autowrap, so text wrapping must reserve that same column as well.
    let available = width.saturating_sub(prefix_width + 1).max(4);
    // `AsciiSpace` keeps links and paths intact so they fold to the next row as
    // one word; `break_words` is the last resort for a word wider than the row.
    let options = textwrap::Options::new(available)
        .break_words(true)
        .word_separator(textwrap::WordSeparator::AsciiSpace);
    let expanded = expand_tabs(text);
    let wrapped = textwrap::wrap(expanded.as_ref(), options);
    if wrapped.is_empty() {
        return vec![PaintLine {
            prefix: prefix.to_owned(),
            prefix_tone,
            text: String::new(),
            tone,
            bold,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        }];
    }

    let part_count = wrapped.len();
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, part)| PaintLine {
            prefix: if index == 0 {
                prefix.to_owned()
            } else {
                continuation.to_owned()
            },
            prefix_tone,
            text: part.into_owned(),
            tone,
            bold,
            tool_heading: None,
            pick: None,
            tail: if index + 1 < part_count {
                vec![PaintSpan {
                    text: String::new(),
                    tone: Tone::CopyJoin,
                    bold: false,
                }]
            } else {
                Vec::new()
            },
        })
        .collect()
}

fn copy_joins_next(line: &PaintLine) -> bool {
    line.tail.iter().any(|span| span.tone == Tone::CopyJoin)
}

#[cfg(test)]
fn composer_display(editor: &Editor, composer_images: &[String]) -> (String, usize) {
    let (display, cursor, _) = composer_display_with_spans(editor, composer_images);
    (display, cursor)
}

/// The composer text as painted, its cursor, and the composer characters each
/// painted character stands for. An image label and a collapsed-paste summary are
/// each one unit: every character of them answers to the whole span behind them,
/// so a drag that touches any part deletes the attachment or the paste whole.
/// Padding around the labels stands for nothing and carries an empty span.
fn composer_display_with_spans(
    editor: &Editor,
    composer_images: &[String],
) -> (String, usize, Vec<Range<usize>>) {
    let labels = (1..=composer_images.len())
        .map(|index| format!("[Image #{index}]"))
        .collect::<Vec<_>>();
    let (source, source_cursor, source_spans) = composer_source_spans(editor);
    let chars = source.chars().collect::<Vec<_>>();
    let mut display = String::new();
    let mut spans: Vec<Range<usize>> = Vec::new();
    let mut display_cursor = 0;
    let mut image_index = 0;
    for (index, ch) in chars.iter().copied().enumerate() {
        if index == source_cursor {
            display_cursor = display.chars().count();
        }
        let span = source_spans
            .get(index)
            .cloned()
            .unwrap_or_else(|| index..index + 1);
        if ch != ATTACHMENT_PLACEHOLDER {
            display.push(ch);
            spans.push(span);
            continue;
        }
        let Some(label) = labels.get(image_index) else {
            continue;
        };
        if display.chars().last().is_some_and(|ch| !ch.is_whitespace()) {
            display.push(' ');
            spans.push(span.start..span.start);
        }
        display.push_str(label);
        spans.extend(label.chars().map(|_| span.clone()));
        if chars
            .get(index + 1)
            .is_some_and(|&next| next != ATTACHMENT_PLACEHOLDER && !next.is_whitespace())
        {
            display.push(' ');
            spans.push(span.end..span.end);
        }
        image_index += 1;
    }
    if source_cursor == chars.len() {
        display_cursor = display.chars().count();
    }
    if image_index < labels.len() {
        // Attachments with no placeholder of their own: nothing in the composer
        // stands behind these labels, so nothing can be deleted through them.
        let end = editor.chars().len();
        if !display.is_empty() {
            display.push(' ');
            spans.push(end..end);
        }
        let trailing = labels[image_index..].join(" ");
        spans.extend(trailing.chars().map(|_| end..end));
        display.push_str(&trailing);
    }
    (display, display_cursor, spans)
}

/// The composer's own text before image labels, with the characters behind each
/// of its characters. A collapsed paste shows as one summary, so every character
/// of that summary answers to the whole pasted block.
fn composer_source_spans(editor: &Editor) -> (String, usize, Vec<Range<usize>>) {
    let buffer_len = editor.chars().len();
    let Some(((source, cursor), paste)) = editor
        .collapsed_paste_display()
        .zip(editor.collapsed_paste_range())
    else {
        let text = editor.display_text();
        let spans = (0..text.chars().count())
            .map(|index| index..index + 1)
            .collect();
        return (text, editor.display_cursor(), spans);
    };
    let source_len = source.chars().count();
    let tail_len = buffer_len.saturating_sub(paste.end);
    let summary_len = source_len.saturating_sub(paste.start + tail_len);
    let mut spans = Vec::with_capacity(source_len);
    spans.extend((0..paste.start).map(|index| index..index + 1));
    spans.extend((0..summary_len).map(|_| paste.clone()));
    spans.extend((paste.end..paste.end + tail_len).map(|index| index..index + 1));
    (source, cursor, spans)
}

/// Blocks whose lines carry `devez-copy-v1` metadata. Excludes the ones drawn as
/// cards, whose box art is content the reader asked to see.
fn copy_metadata_applies(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::Assistant
            | BlockKind::Reasoning
            | BlockKind::Tool
            | BlockKind::FileChange
            | BlockKind::Diff
            | BlockKind::Warning
            | BlockKind::Error
            | BlockKind::System
    )
}

/// Lays a free-text answer out on the option row it is typed on: the rows it
/// takes, and where the cursor sits in them. A newline has nowhere to go on a
/// numbered row, so it reads as the space it separates words with.
fn inline_answer_rows(
    editor: &Editor,
    prefix_width: usize,
    wrap_width: u16,
) -> (Vec<String>, usize, usize) {
    let text = editor.text().replace('\n', " ");
    let content_width = (wrap_width as usize)
        .saturating_sub(prefix_width + 1)
        .max(4);
    let cursor_index = editor.cursor();
    let mut rows = vec![String::new()];
    let mut cursor_row = 0;
    let mut cursor_column = prefix_width;
    let mut column = prefix_width;
    for (index, ch) in text.chars().enumerate() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if column + width > prefix_width + content_width && !rows[cursor_last(&rows)].is_empty() {
            rows.push(String::new());
            column = prefix_width;
        }
        if index == cursor_index {
            cursor_row = rows.len() - 1;
            cursor_column = column;
        }
        let row = cursor_last(&rows);
        rows[row].push(ch);
        column += width;
    }
    if cursor_index >= text.chars().count() {
        cursor_row = rows.len() - 1;
        cursor_column = column;
    }
    (rows, cursor_row, cursor_column)
}

fn cursor_last(rows: &[String]) -> usize {
    rows.len() - 1
}

fn input_lines(
    editor: &Editor,
    composer_images: &[String],
    width: u16,
    label: &str,
    placeholder: &str,
    notice: Option<&str>,
    mode: Option<&ComposerMode>,
) -> (Vec<PaintLine>, usize, usize, ComposerLayout) {
    input_lines_with_controls(
        editor,
        composer_images,
        width,
        label,
        placeholder,
        notice,
        mode,
        mode,
    )
}

fn input_lines_with_controls(
    editor: &Editor,
    composer_images: &[String],
    width: u16,
    label: &str,
    placeholder: &str,
    notice: Option<&str>,
    mode: Option<&ComposerMode>,
    controls_mode: Option<&ComposerMode>,
) -> (Vec<PaintLine>, usize, usize, ComposerLayout) {
    // Windows IME composes Hangul in the terminal before the committed character
    // reaches us. A wide preedit also keeps a cursor cell while it moves to the
    // next visual row, so leave one additional blank cell before the closing
    // border. This prevents the transient glyph from flashing at the new row's
    // right edge before the committed syllable is painted.
    const COMPOSER_IME_RIGHT_GUTTER: usize = 4;
    let (display, editor_cursor, display_spans) =
        composer_display_with_spans(editor, composer_images);
    let display_chars = display.chars().collect::<Vec<_>>();
    let panel_width = (width as usize).saturating_sub(1).max(16);
    let side_prefix = "│ ";
    let first_prefix = "> ";
    let continuation_prefix = "  ";
    let content_width = panel_width
        .saturating_sub(
            UnicodeWidthStr::width(side_prefix)
                + UnicodeWidthStr::width(first_prefix)
                + 1
                + COMPOSER_IME_RIGHT_GUTTER,
        )
        .max(4);
    let mut raw_rows = vec![String::new()];
    let mut row_glyphs: Vec<Vec<ComposerGlyph>> = vec![Vec::new()];
    // One row exists before the first glyph lands, so the vec starts holding
    // that row's empty range rather than a range of rows.
    #[allow(clippy::single_range_in_vec_init)]
    let mut row_ranges = vec![0..0];
    let mut row = 0;
    let input_prefix_width =
        UnicodeWidthStr::width(side_prefix) + UnicodeWidthStr::width(first_prefix);
    let mut column = input_prefix_width;
    let mut cursor_row = 0;
    let mut cursor_column = column;

    for (index, ch) in display_chars.iter().copied().enumerate() {
        if index == editor_cursor {
            cursor_row = row;
            cursor_column = column;
        }
        let span = display_spans
            .get(index)
            .cloned()
            .unwrap_or_else(|| index..index + 1);

        if ch == '\n' {
            row_ranges[row].end = span.start;
            raw_rows.push(String::new());
            row_glyphs.push(Vec::new());
            row_ranges.push(span.end..span.end);
            row += 1;
            column = input_prefix_width;
            continue;
        }

        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        let content_column = column.saturating_sub(input_prefix_width);
        if content_column + ch_width > content_width && !raw_rows[row].is_empty() {
            row_ranges[row].end = span.start;
            raw_rows.push(String::new());
            row_glyphs.push(Vec::new());
            row_ranges.push(span.start..span.start);
            row += 1;
            column = input_prefix_width;
            if index == editor_cursor {
                cursor_row = row;
                cursor_column = column;
            }
        }
        raw_rows[row].push(ch);
        row_ranges[row].end = span.end;
        // A combining mark rides along with the glyph it attaches to, so it is
        // deleted with it rather than being left behind on its own.
        match row_glyphs[row].last_mut() {
            Some(last) if ch_width == 0 && span.start < span.end => {
                last.span.start = last.span.start.min(span.start);
                last.span.end = last.span.end.max(span.end);
            }
            _ => row_glyphs[row].push(ComposerGlyph {
                width: ch_width,
                span,
            }),
        }
        column += ch_width;
    }

    if editor_cursor == display_chars.len() {
        cursor_row = row;
        cursor_column = column;
    }

    // Past this the composer stops growing and scrolls inside itself, the way
    // Claude Code's does, so a long draft cannot push the transcript off screen.
    // The window follows the cursor row.
    let visible_start = raw_rows
        .len()
        .saturating_sub(COMPOSER_MAX_PROMPT_ROWS)
        .min(cursor_row.saturating_sub(COMPOSER_MAX_PROMPT_ROWS - 1));
    let visible_end = (visible_start + COMPOSER_MAX_PROMPT_ROWS).min(raw_rows.len());

    let mut rows = Vec::with_capacity(visible_end - visible_start + 2);
    rows.push(input_top_line_with_controls(
        panel_width,
        label,
        mode,
        controls_mode,
    ));
    let chrome_tone = composer_chrome_tone(mode);
    for (index, raw) in raw_rows
        .into_iter()
        .enumerate()
        .skip(visible_start)
        .take(visible_end - visible_start)
    {
        let is_placeholder = editor.is_empty() && composer_images.is_empty() && index == 0;
        let content = if is_placeholder {
            placeholder.to_owned()
        } else {
            raw
        };
        let prompt_prefix = if index == 0 {
            first_prefix
        } else {
            continuation_prefix
        };
        let content_width = UnicodeWidthStr::width(content.as_str());
        let content_tone = if is_placeholder {
            Tone::Muted
        } else {
            Tone::Plain
        };
        rows.push(PaintLine {
            prefix: side_prefix.to_owned(),
            prefix_tone: chrome_tone,
            text: prompt_prefix.to_owned(),
            tone: chrome_tone,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: vec![
                PaintSpan {
                    text: content,
                    tone: content_tone,
                    bold: false,
                },
                PaintSpan {
                    text: " ".repeat(panel_width.saturating_sub(
                        UnicodeWidthStr::width(side_prefix)
                            + UnicodeWidthStr::width(prompt_prefix)
                            + content_width
                            + 1,
                    )),
                    tone: chrome_tone,
                    bold: false,
                },
                PaintSpan {
                    text: "│".to_owned(),
                    tone: chrome_tone,
                    bold: false,
                },
            ],
        });
    }
    // Both composer rules share the welcome card's border colour, so the frame
    // around the prompt reads as the same furniture the panels are drawn from.
    rows.push(input_bottom_line(panel_width, notice, mode));

    let layout = ComposerLayout {
        rows: row_glyphs
            .into_iter()
            .zip(row_ranges)
            .skip(visible_start)
            .take(visible_end - visible_start)
            .map(|(glyphs, range)| ComposerRowLayout {
                start_column: input_prefix_width,
                start: range.start,
                end: range.end,
                glyphs,
            })
            .collect(),
    };
    (rows, cursor_row - visible_start + 1, cursor_column, layout)
}

/// Prompt rows the composer paints before it starts scrolling instead of growing.
const COMPOSER_MAX_PROMPT_ROWS: usize = 10;

/// Shortest rule stub kept on the composer top line so the frame never collapses.
const COMPOSER_RULE_MIN: usize = 4;
/// Blank columns between the rule and the mode badge.
const COMPOSER_MODE_GAP: usize = 2;
/// Rule segment trailing the mode badge, so the line reads as unbroken.
const COMPOSER_MODE_TAIL_RULE: usize = 2;
/// Blank columns between the bottom rule and a transient notice.
const COMPOSER_NOTICE_GAP: usize = 2;
/// Rule segment trailing a transient notice at the right edge.
const COMPOSER_NOTICE_TAIL_RULE: usize = 2;
/// Separator between the permission mode and the fast-tier flag.
const COMPOSER_BADGE_SEPARATOR: &str = " · ";

/// Rule the composer's top line opens with when it carries a label.
const OPENING_RULE: &str = "── ";

#[cfg(test)]
fn input_top_line(panel_width: usize, label: &str, mode: Option<&ComposerMode>) -> PaintLine {
    input_top_line_with_controls(panel_width, label, mode, mode)
}

fn input_top_line_with_controls(
    panel_width: usize,
    label: &str,
    mode: Option<&ComposerMode>,
    controls_mode: Option<&ComposerMode>,
) -> PaintLine {
    let chrome_tone = composer_chrome_tone(mode);
    let left = if !label.is_empty() {
        format!("{OPENING_RULE}{label} ")
    } else {
        Default::default()
    };
    let left_width = UnicodeWidthStr::width(left.as_str());
    // Right-hand badges eat into this budget; whatever survives stays as rule.
    let mut budget = panel_width.saturating_sub(left_width + COMPOSER_RULE_MIN);
    // A bare rule paints as one span before the tail; a labelled one spends three
    // — the opening stroke, the label, then the fill.
    let tail_offset = if left.is_empty() { 1 } else { 3 };

    // The mode is persistent state, so it anchors the far right.
    let badge = controls_mode.and_then(|mode| {
        // Blanks either side of the badge plus the rule stub that trails it.
        let reserved = COMPOSER_MODE_GAP + 1 + COMPOSER_MODE_TAIL_RULE;
        let badge = fitting_badge_spans(mode, budget.saturating_sub(reserved))?;
        budget -= spans_width(&badge.spans) + reserved;
        Some(badge)
    });

    let mut tail = Vec::new();
    // Where the badge lands once the rule ahead of it is counted: the gap span
    // sits between, and a labelled rule spends one more span on the label.
    let mut picks = Vec::new();
    if let Some(badge) = badge {
        let badge_start = tail_offset + 1;
        picks.extend(
            badge
                .shell_display_mode_index
                .map(|index| (badge_start + index, Pick::ShellDisplayMode)),
        );
        picks.extend(
            badge
                .diff_display_mode_index
                .map(|index| (badge_start + index, Pick::DiffDisplayMode)),
        );
        picks.extend(
            badge
                .response_length_index
                .map(|index| (badge_start + index, Pick::VibeMode)),
        );
        picks.extend(
            badge
                .fast_index
                .map(|index| (badge_start + index, Pick::FastMode)),
        );
        picks.extend(
            badge
                .permission_index
                .map(|index| (badge_start + index, Pick::ClaudePermissionMode)),
        );
        tail.push(rule_gap(COMPOSER_MODE_GAP));
        tail.extend(badge.spans);
        tail.push(rule_gap(1));
        tail.push(PaintSpan {
            text: "─".repeat(COMPOSER_MODE_TAIL_RULE),
            tone: chrome_tone,
            bold: false,
        });
    }

    let fill = "─".repeat((COMPOSER_RULE_MIN + budget).min(panel_width.saturating_sub(left_width)));
    // A label sits inside the stroke rather than replacing it, so the rule either
    // side of it keeps the border colour while the label itself stays muted —
    // the same split `panel_rule_row` uses for a card title.
    if left.is_empty() {
        return corner_composer_rule(
            PaintLine {
                prefix: String::new(),
                prefix_tone: chrome_tone,
                text: fill,
                tone: chrome_tone,
                bold: false,
                tool_heading: None,
                pick: None,
                tail,
            }
            .with_picks(&picks),
            '╭',
            '╮',
        );
    }
    let spans = [
        PaintSpan {
            text: left[OPENING_RULE.len()..].to_owned(),
            tone: Tone::Muted,
            bold: false,
        },
        PaintSpan {
            text: fill,
            tone: chrome_tone,
            bold: false,
        },
    ];
    corner_composer_rule(
        PaintLine {
            prefix: String::new(),
            prefix_tone: chrome_tone,
            text: OPENING_RULE.to_owned(),
            tone: chrome_tone,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: spans.into_iter().chain(tail).collect(),
        }
        .with_picks(&picks),
        '╭',
        '╮',
    )
}

fn input_bottom_line(
    panel_width: usize,
    notice: Option<&str>,
    mode: Option<&ComposerMode>,
) -> PaintLine {
    let chrome_tone = composer_chrome_tone(mode);
    let Some(notice) = notice else {
        return corner_composer_rule(
            PaintLine {
                prefix: String::new(),
                prefix_tone: chrome_tone,
                text: "─".repeat(panel_width),
                tone: chrome_tone,
                bold: false,
                tool_heading: None,
                pick: None,
                tail: Vec::new(),
            },
            '╰',
            '╯',
        );
    };

    let reserved = COMPOSER_NOTICE_GAP + 1 + COMPOSER_NOTICE_TAIL_RULE;
    let notice = compact_right(notice, panel_width.saturating_sub(reserved));
    let fill =
        "─".repeat(panel_width.saturating_sub(UnicodeWidthStr::width(notice.as_str()) + reserved));
    corner_composer_rule(
        PaintLine {
            prefix: String::new(),
            prefix_tone: chrome_tone,
            text: fill,
            tone: chrome_tone,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: vec![
                rule_gap(COMPOSER_NOTICE_GAP),
                PaintSpan {
                    text: notice,
                    tone: Tone::Accent,
                    bold: false,
                },
                rule_gap(1),
                PaintSpan {
                    text: "─".repeat(COMPOSER_NOTICE_TAIL_RULE),
                    tone: chrome_tone,
                    bold: false,
                },
            ],
        },
        '╰',
        '╯',
    )
}

fn composer_chrome_tone(mode: Option<&ComposerMode>) -> Tone {
    mode.and_then(|mode| model_tone(&mode.model))
        .unwrap_or(Tone::Border)
}

/// Turns the outermost rule cells into the same closed corners the welcome card
/// uses, without changing the width or shifting clickable composer badges.
fn corner_composer_rule(mut line: PaintLine, left: char, right: char) -> PaintLine {
    if line.text.starts_with('─') {
        line.text
            .replace_range(0..'─'.len_utf8(), &left.to_string());
    }
    let last = line
        .tail
        .iter_mut()
        .rev()
        .find(|span| !span.text.is_empty())
        .map(|span| &mut span.text)
        .unwrap_or(&mut line.text);
    if let Some((index, _)) = last.char_indices().last() {
        last.replace_range(index.., &right.to_string());
    }
    line
}

/// Widest badge that fits in `budget`: response length · Shell display mode ·
/// Diff display mode · fast flag. Tightening drops optional trailing controls;
/// response length remains. Parts
/// are never ellipsized — a half-written label or clipped price is worse than none.
fn fitting_badge_spans(mode: &ComposerMode, budget: usize) -> Option<BadgeSpans> {
    let mut display_spans = Vec::new();
    if let Some(branch) = mode.branch.as_deref().filter(|branch| !branch.is_empty()) {
        display_spans.push(PaintSpan {
            text: format!("* {branch}"),
            tone: Tone::Branch,
            bold: false,
        });
        display_spans.push(display_separator_span());
    }
    let display_width = display_spans.len();
    let vibe_mode_span = PaintSpan {
        text: mode.vibe_mode.clone(),
        tone: vibe_tone(mode.vibe_tone),
        bold: false,
    };
    let fast_label = if mode.fast_mode {
        "Fast: On"
    } else {
        "Fast: Off"
    };
    let fast_span = PaintSpan {
        text: fast_label.to_owned(),
        tone: if mode.fast_mode {
            Tone::FastOn
        } else {
            Tone::FastOff
        },
        bold: false,
    };

    let custom_spans = false.then(|| {
        vec![
            PaintSpan {
                text: format!("Response: {}", mode.response_length),
                tone: Tone::Muted,
                bold: false,
            },
            separator_span(),
            PaintSpan {
                text: format!("Shell: {}", mode.shell_display_mode),
                tone: Tone::Muted,
                bold: false,
            },
            separator_span(),
            PaintSpan {
                text: format!("Diff: {}", mode.diff_display_mode),
                tone: Tone::Muted,
                bold: false,
            },
        ]
    });
    let primary_spans = custom_spans.unwrap_or_else(|| vec![vibe_mode_span.clone()]);
    let without_fast = BadgeSpans {
        spans: [display_spans.clone(), primary_spans.clone()].concat(),
        response_length_index: Some(display_width),
        shell_display_mode_index: None,
        diff_display_mode_index: None,
        fast_index: None,
        permission_index: None,
    };
    if mode.model.starts_with("claude:") {
        // Claude has no service tier to flip; the permission mode takes the slot
        // Fast holds on a Codex thread, and drops first when the rule tightens.
        let ladder = mode
            .claude_permission
            .iter()
            .map(|permission| BadgeSpans {
                spans: [
                    display_spans.clone(),
                    [primary_spans.clone(), vec![separator_span()]].concat(),
                    vec![PaintSpan {
                        text: permission.label.clone(),
                        tone: permission_tone(permission.tone),
                        bold: false,
                    }],
                ]
                .concat(),
                response_length_index: Some(display_width),
                shell_display_mode_index: None,
                diff_display_mode_index: None,
                fast_index: None,
                permission_index: Some(display_width + primary_spans.len() + 1),
            })
            .chain(std::iter::once(without_fast));
        return ladder
            .into_iter()
            .find(|candidate| spans_width(&candidate.spans) <= budget);
    }

    // Fast is the only optional trailing control for models that expose it.
    let ladder = [
        BadgeSpans {
            spans: [
                display_spans.clone(),
                [primary_spans.clone(), vec![separator_span()]].concat(),
                vec![fast_span.clone()],
            ]
            .concat(),
            response_length_index: Some(display_width),
            shell_display_mode_index: None,
            diff_display_mode_index: None,
            fast_index: Some(display_width + primary_spans.len() + 1),
            permission_index: None,
        },
        without_fast,
    ];
    ladder
        .into_iter()
        .find(|candidate| spans_width(&candidate.spans) <= budget)
}

/// The badge as painted, plus where its two clickable parts sit inside it. The
/// separators and the cost shift those positions around, and only the ladder
/// that picked the candidate knows which rung it settled on.
struct BadgeSpans {
    spans: Vec<PaintSpan>,
    response_length_index: Option<usize>,
    shell_display_mode_index: Option<usize>,
    diff_display_mode_index: Option<usize>,
    fast_index: Option<usize>,
    permission_index: Option<usize>,
}

fn permission_tone(tone: PermissionTone) -> Tone {
    match tone {
        PermissionTone::Neutral => Tone::FastOff,
        PermissionTone::AcceptEdits => Tone::ClaudeAcceptEdits,
        PermissionTone::Plan => Tone::ClaudePlan,
        PermissionTone::Auto => Tone::ClaudeAuto,
        PermissionTone::Bypass => Tone::ClaudeBypass,
    }
}

fn separator_span() -> PaintSpan {
    PaintSpan {
        text: COMPOSER_BADGE_SEPARATOR.to_owned(),
        tone: Tone::Muted,
        bold: false,
    }
}

fn display_separator_span() -> PaintSpan {
    PaintSpan {
        text: " | ".to_owned(),
        tone: Tone::Muted,
        bold: false,
    }
}

fn spans_width(spans: &[PaintSpan]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum()
}

/// The status line's colour for a reasoning effort, shared with the effort
/// slider so a tier reads the same wherever it appears. `None` for ids the
/// palette has no ramp entry for.
fn effort_tone(effort: &str) -> Option<Tone> {
    Some(match effort {
        "Off" => vibe_tone(VibeTone::Off),
        "On" => vibe_tone(VibeTone::On),
        "Super Vibe" => vibe_tone(VibeTone::Super),
        "low" => Tone::EffortLow,
        "medium" => Tone::EffortMedium,
        "high" => Tone::EffortHigh,
        "xhigh" => Tone::EffortXHigh,
        "max" => Tone::EffortMax,
        "ultra" => Tone::EffortUltra,
        _ => return None,
    })
}

fn vibe_tone(vibe: VibeTone) -> Tone {
    match vibe {
        VibeTone::Off => Tone::Muted,
        VibeTone::On => Tone::FastOn,
        VibeTone::Super => Tone::VibeSuper,
    }
}

fn rule_gap(width: usize) -> PaintSpan {
    PaintSpan {
        text: " ".repeat(width),
        tone: Tone::Muted,
        bold: false,
    }
}

#[allow(dead_code)]
fn mode_accent_tone(accent: ModeAccent) -> Tone {
    match accent {
        ModeAccent::Calm => Tone::Muted,
        // Not `Context`: that tone carries the status line's contrast-exempt
        // emerald, and this badge sits on the composer rule instead.
        ModeAccent::Safe => Tone::Success,
        ModeAccent::Danger => Tone::Warning,
    }
}

fn compact_text(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    if max_width <= 1 {
        return "…".to_owned();
    }
    let mut output = String::new();
    let mut width = 0;
    for ch in text.chars().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width >= max_width {
            break;
        }
        output.insert(0, ch);
        width += ch_width;
    }
    format!("…{output}")
}

fn compact_right(text: &str, max_width: usize) -> String {
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

fn print_line(out: &mut impl Write, line: &PaintLine) -> Result<()> {
    print_line_with_selection(out, line, None, None)
}

fn paint_scroll_to_bottom_into_frame(
    frame: &mut CellFrame,
    row: usize,
    control: &PaintLine,
    hovered: bool,
) {
    let start = UnicodeWidthStr::width(control.prefix.as_str());
    frame.write(
        start,
        row,
        &control.text,
        cell_style(
            control.tone,
            false,
            Some(scroll_to_bottom_background(hovered)),
            false,
        ),
    );
}

/// Where the streamed text ends on screen, and how many characters behind that
/// point are still settling.
#[derive(Clone, Copy)]
pub struct StreamFade {
    /// The last row the live blocks occupy.
    pub last_row: usize,
    pub tail: usize,
}

/// A character arriving at full strength is a hard edge, and at fifty characters
/// a second the eye reads a row of hard edges as stutter however evenly they are
/// spaced. Bringing the newest few up from the background turns each arrival into
/// a rise instead.
///
/// The tail is walked backwards from the end of the text, and a run of box-drawing
/// glyphs ends it: past that lies a bubble edge or a rule, which is furniture
/// rather than text and has no business dimming.
fn fade_stream_tail_into_frame(frame: &mut CellFrame, fade: StreamFade) {
    if fade.tail == 0 {
        return;
    }
    let background = theme::palette().background;
    let mut faded = 0usize;
    if frame.height == 0 {
        return;
    }
    for row in (0..=fade.last_row.min(frame.height - 1)).rev() {
        for column in (0..frame.width).rev() {
            let cell = frame.cell_mut(column, row);
            if cell.continuation || cell.glyph.trim().is_empty() {
                continue;
            }
            if is_box_drawing(&cell.glyph) {
                return;
            }
            let Some(foreground) = cell.style.foreground else {
                continue;
            };
            // The newest character sits deepest in the background and each one
            // before it stands a step closer to full strength.
            let level = 255 - (255 * (faded + 1) / (fade.tail + 1)) as u8;
            cell.style.foreground = Some(blend(foreground, background, level));
            faded += 1;
            if faded >= fade.tail {
                return;
            }
        }
    }
}

fn is_box_drawing(glyph: &str) -> bool {
    glyph
        .chars()
        .next()
        .is_some_and(|ch| matches!(u32::from(ch), 0x2500..=0x259f))
}

/// Whether a piece of the row painted across `start..end` falls under the
/// hovered columns. Every clickable span was measured off the painted row, so a
/// piece is either inside the highlight or outside it, never half-lit.
fn set_piece_style(
    out: &mut impl Write,
    background: Option<Rgb>,
    tone: Tone,
    bold: bool,
) -> Result<()> {
    queue!(
        out,
        SetBackgroundColor(background.map_or(Color::Reset, rgb_color))
    )?;
    set_tone(out, tone)?;
    if bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    Ok(())
}

/// Prints one run of a row, split where the hover highlight starts and stops. A
/// run is not an all-or-nothing unit the way a selection block is: the highlight
/// reaches a column past the text it belongs to, and the separator column it
/// reaches into must light up by that one column and no further.
fn print_hovered_chunks(
    out: &mut impl Write,
    text: &str,
    column: &mut usize,
    selected_columns: Option<&Range<usize>>,
    hovered_columns: Option<&Range<usize>>,
    tone: Tone,
    bold: bool,
    background: Option<Rgb>,
) -> Result<()> {
    if hovered_columns.is_none() {
        set_piece_style(out, background, tone, bold)?;
        return print_selected_chunks(out, text, column, selected_columns, tone, bold, background);
    }

    let hover_bg = theme::palette().hover_bg;
    for chunk in selection_chunks(text, *column, hovered_columns.cloned()) {
        let chunk_background = if chunk.selected {
            Some(hover_bg)
        } else {
            background
        };
        set_piece_style(out, chunk_background, tone, bold)?;
        print_selected_chunks(
            out,
            &chunk.text,
            column,
            selected_columns,
            tone,
            bold,
            chunk_background,
        )?;
    }
    Ok(())
}

/// The cells that have to be redrawn when a hover moves. Keeping disjoint
/// badges separate avoids clearing the controls between them, which was visible
/// as a blink while crossing the composer rule.
#[allow(dead_code)]
fn hover_repaint_columns(
    previously_hovered: Option<Range<usize>>,
    hovered: Option<Range<usize>>,
) -> Vec<Range<usize>> {
    let mut ranges = [previously_hovered, hovered]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| range.start);

    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

/// The band a row carries all the way to the right edge. Word tones resolve to
/// their row's band here: they mark a run *inside* an added/removed row, so the
/// row they wrapped out of must still paint the same full-width tint.
fn row_background(tone: Tone) -> Option<Rgb> {
    let palette = theme::palette();
    Some(match tone {
        Tone::UserPrompt | Tone::UserPromptPadding if !CHAT_LAYOUT.load(Ordering::Relaxed) => {
            palette.user_prompt_bg
        }
        Tone::ModelChange => palette.model_change_bg,
        Tone::DiffAdded | Tone::DiffAddedWord => palette.diff_add_bg,
        Tone::DiffRemoved | Tone::DiffRemovedWord => palette.diff_remove_bg,
        _ => return None,
    })
}

fn assistant_bubble_background() -> Rgb {
    let palette = theme::palette();
    blend(palette.background, palette.foreground, 20)
}

/// Assistant rows carry a zero-width sentinel so every colored span keeps the
/// same bubble band without changing its foreground theme color.
fn bubble_background(line: &PaintLine) -> Option<Rgb> {
    line.tail
        .iter()
        .any(|span| span.tone == Tone::AssistantBubble && span.text.is_empty())
        .then(assistant_bubble_background)
}

/// The stronger tint a single run gets on top of its row's band, for the words a
/// diff row actually changed.
fn word_background(tone: Tone) -> Option<Rgb> {
    let palette = theme::palette();
    Some(match tone {
        Tone::UserPrompt | Tone::UserPromptPadding if CHAT_LAYOUT.load(Ordering::Relaxed) => {
            palette.user_prompt_bg
        }
        Tone::AssistantBubble | Tone::AssistantBubbleHalf => assistant_bubble_background(),
        Tone::DiffAddedWord => palette.diff_add_word_bg,
        Tone::DiffRemovedWord => palette.diff_remove_word_bg,
        // The status-line model reading stays flat at rest; only the hover pass
        // paints a band behind it.
        _ => return None,
    })
}

fn print_line_with_selection(
    out: &mut impl Write,
    line: &PaintLine,
    selected_columns: Option<Range<usize>>,
    hovered_columns: Option<Range<usize>>,
) -> Result<()> {
    print_line_with_selection_bounded(out, line, selected_columns, hovered_columns, None)
}

/// Paints a whole row, but stops a row background at `background_width` when
/// the fullscreen renderer has docked a right-side panel. `Clear(UntilNewLine)`
/// would otherwise repaint the gap and panel cells after their layout was fixed.
fn print_line_with_selection_bounded(
    out: &mut impl Write,
    line: &PaintLine,
    selected_columns: Option<Range<usize>>,
    hovered_columns: Option<Range<usize>>,
    background_width: Option<usize>,
) -> Result<()> {
    let background = row_background(line.tone);
    let bubble_background = bubble_background(line);
    if let Some(background) = background {
        queue!(out, SetBackgroundColor(rgb_color(background)))?;
    }
    let mut column = 0;
    print_hovered_chunks(
        out,
        &line.prefix,
        &mut column,
        selected_columns.as_ref(),
        hovered_columns.as_ref(),
        line.prefix_tone,
        false,
        word_background(line.prefix_tone)
            .or(bubble_background)
            .or(background),
    )?;
    print_hovered_chunks(
        out,
        &line.text,
        &mut column,
        selected_columns.as_ref(),
        hovered_columns.as_ref(),
        line.tone,
        line.bold,
        word_background(line.tone)
            .or(bubble_background)
            .or(background),
    )?;
    queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    for span in &line.tail {
        if span.tone == Tone::CopyJoin {
            continue;
        }
        // A tail span inherits the row's band, so a wrapped diff row stays tinted
        // to the right edge and its word tints keep landing on the right words.
        print_hovered_chunks(
            out,
            &span.text,
            &mut column,
            selected_columns.as_ref(),
            hovered_columns.as_ref(),
            span.tone,
            span.bold,
            word_background(span.tone)
                .or(bubble_background)
                .or(background),
        )?;
        queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    }
    if let Some(background) = background {
        queue!(out, SetBackgroundColor(rgb_color(background)))?;
        if let Some(width) = background_width {
            queue!(out, Print(" ".repeat(width.saturating_sub(column))))?;
        } else {
            queue!(out, Clear(ClearType::UntilNewLine))?;
        }
        queue!(out, ResetColor)?;
    }
    Ok(())
}

/// Repaints just `columns` of an otherwise unchanged row. This is used for a
/// hover transition, where clearing and rewriting the entire composer rule
/// makes every neighbouring badge visibly blink.
#[allow(dead_code)]
fn print_line_columns(
    out: &mut impl Write,
    line: &PaintLine,
    selected_columns: Option<Range<usize>>,
    hovered_columns: Option<Range<usize>>,
    columns: Range<usize>,
) -> Result<()> {
    let background = row_background(line.tone);
    let bubble_background = bubble_background(line);
    let mut column = 0;
    print_line_columns_piece(
        out,
        &line.prefix,
        &mut column,
        &columns,
        selected_columns.as_ref(),
        hovered_columns.as_ref(),
        line.prefix_tone,
        false,
        word_background(line.prefix_tone)
            .or(bubble_background)
            .or(background),
    )?;
    print_line_columns_piece(
        out,
        &line.text,
        &mut column,
        &columns,
        selected_columns.as_ref(),
        hovered_columns.as_ref(),
        line.tone,
        line.bold,
        word_background(line.tone)
            .or(bubble_background)
            .or(background),
    )?;
    for span in &line.tail {
        if span.tone == Tone::CopyJoin {
            continue;
        }
        print_line_columns_piece(
            out,
            &span.text,
            &mut column,
            &columns,
            selected_columns.as_ref(),
            hovered_columns.as_ref(),
            span.tone,
            span.bold,
            word_background(span.tone)
                .or(bubble_background)
                .or(background),
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
fn print_line_columns_piece(
    out: &mut impl Write,
    text: &str,
    column: &mut usize,
    columns: &Range<usize>,
    selected_columns: Option<&Range<usize>>,
    hovered_columns: Option<&Range<usize>>,
    tone: Tone,
    bold: bool,
    background: Option<Rgb>,
) -> Result<()> {
    for chunk in selection_chunks(text, *column, Some(columns.clone())) {
        if chunk.selected {
            print_hovered_chunks(
                out,
                &chunk.text,
                column,
                selected_columns,
                hovered_columns,
                tone,
                bold,
                background,
            )?;
            queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
        } else {
            *column += UnicodeWidthStr::width(chunk.text.as_str());
        }
    }
    Ok(())
}

/// Paints one run of a row, split where the drag selection starts and ends.
///
/// The highlight is one fixed block rather than reverse video. A reversed cell
/// borrows whatever colours surround it, so the same drag came out charcoal on
/// Dark, cream on Soft, and a different shade again over a user prompt or a
/// diff row. Instead each theme names one selection tone — `selection_bg` —
/// carried over from DevezCode's Claude Code themes, and the whole drag gets
/// that block no matter what it sits on.
fn print_selected_chunks(
    out: &mut impl Write,
    text: &str,
    column: &mut usize,
    selected_columns: Option<&Range<usize>>,
    tone: Tone,
    bold: bool,
    background: Option<Rgb>,
) -> Result<()> {
    let Some(selected_columns) = selected_columns else {
        queue!(out, Print(text))?;
        *column += UnicodeWidthStr::width(text);
        return Ok(());
    };

    let mut highlighted = false;
    for chunk in selection_chunks(text, *column, Some(selected_columns.clone())) {
        if chunk.selected != highlighted {
            set_selection_style(out, chunk.selected, tone, bold, background)?;
            highlighted = chunk.selected;
        }
        queue!(out, Print(chunk.text))?;
    }
    // The row's own colours have to be back in place before the caller prints
    // its next run, or a selection that ends mid-row bleeds into the rest.
    if highlighted {
        set_selection_style(out, false, tone, bold, background)?;
    }
    *column += UnicodeWidthStr::width(text);
    Ok(())
}

fn set_selection_style(
    out: &mut impl Write,
    selected: bool,
    tone: Tone,
    bold: bool,
    background: Option<Rgb>,
) -> Result<()> {
    if selected {
        // Text keeps its own colour inside the block, the way Codex paints it;
        // `selection_text` only steps in for runs this theme's block would
        // swallow.
        let text = tone_rgb(tone).map_or_else(theme::selection_fg, theme::selection_text);
        queue!(
            out,
            SetBackgroundColor(rgb_color(theme::selection_bg())),
            SetForegroundColor(rgb_color(text))
        )?;
        return Ok(());
    }
    queue!(
        out,
        SetBackgroundColor(background.map_or(Color::Reset, rgb_color))
    )?;
    set_tone(out, tone)?;
    if bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    Ok(())
}

fn model_tone(model: &str) -> Option<Tone> {
    let model = model.to_ascii_lowercase();
    if model.contains("haiku") {
        Some(Tone::ModelHaiku)
    } else if model.contains("sonnet") {
        Some(Tone::ModelSonnet)
    } else if model.contains("opus") {
        Some(Tone::ModelOpus)
    } else if model.contains("fable") {
        Some(Tone::ModelFable)
    } else if model.contains("spark") {
        Some(Tone::ModelSpark)
    } else if model.contains("5.6") && model.contains("sol") {
        Some(Tone::ModelSol)
    } else if model.contains("5.6") && model.contains("terra") {
        Some(Tone::ModelTerra)
    } else if model.contains("5.6") && model.contains("luna") {
        Some(Tone::ModelLuna)
    } else if model.contains("5.6") {
        Some(Tone::Model56)
    } else if model.contains("5.5") {
        Some(Tone::Model55)
    } else {
        None
    }
}

fn status_model_tone(model: &str) -> Option<Tone> {
    match model_tone(model)? {
        Tone::Model56 => Some(Tone::StatusModel56),
        Tone::ModelSol => Some(Tone::StatusModelSol),
        Tone::ModelTerra => Some(Tone::StatusModelTerra),
        Tone::ModelLuna => Some(Tone::StatusModelLuna),
        Tone::ModelSpark => Some(Tone::StatusModelSpark),
        Tone::Model55 => Some(Tone::StatusModel55),
        Tone::ModelHaiku => Some(Tone::StatusModelHaiku),
        Tone::ModelSonnet => Some(Tone::StatusModelSonnet),
        Tone::ModelOpus => Some(Tone::StatusModelOpus),
        Tone::ModelFable => Some(Tone::StatusModelFable),
        _ => None,
    }
}

fn status_effort_tone(effort: &str) -> Option<Tone> {
    Some(match effort {
        "low" => Tone::StatusEffortLow,
        "medium" => Tone::StatusEffortMedium,
        "high" => Tone::StatusEffortHigh,
        "xhigh" => Tone::StatusEffortXHigh,
        "max" => Tone::StatusEffortMax,
        "ultra" => Tone::StatusEffortUltra,
        _ => return None,
    })
}

/// The colour a tone paints its text in, or `None` for the copy-join marker,
/// which is never printed. Split out of `set_tone` so a selection can ask what
/// a run would have looked like before deciding whether it survives the block.
fn tone_rgb(tone: Tone) -> Option<Rgb> {
    let palette = theme::palette();
    Some(match tone {
        Tone::Plain => palette.foreground,
        Tone::Muted | Tone::Thinking | Tone::PlanDone => palette.muted,
        Tone::Accent => palette.accent,
        Tone::User => palette.blue,
        Tone::ScrollToBottom => palette.foreground,
        Tone::History => blend(palette.foreground, palette.muted, HISTORY_LABEL_MUTED_BLEND),
        Tone::Success => palette.success,
        Tone::Warning => palette.warning,
        Tone::Error => palette.error,
        Tone::Code => palette.code,
        Tone::EffortLow => palette.status.effort_low,
        Tone::EffortMedium => palette.status.effort_medium,
        Tone::EffortHigh => palette.status.effort_high,
        Tone::EffortXHigh => palette.status.effort_xhigh,
        Tone::EffortMax => palette.status.effort_max,
        Tone::EffortUltra => palette.status.effort_ultra,
        Tone::StatusText => palette.status.text,
        Tone::StatusSeparator => palette.status.separator,
        Tone::UserPrompt => palette.foreground,
        Tone::UserPromptPadding => palette.user_prompt_bg,
        Tone::AssistantBubble => palette.foreground,
        Tone::AssistantBubbleHalf => blend(palette.background, palette.foreground, 20),
        Tone::Model56 => palette.model_gpt56,
        Tone::ModelSol => palette.model_sol,
        Tone::ModelTerra => palette.model_terra,
        Tone::ModelLuna => palette.model_luna,
        Tone::ModelSpark => palette.model_spark,
        Tone::Model55 => palette.model_gpt55,
        Tone::ModelHaiku => palette.status.model_haiku,
        Tone::ModelSonnet => palette.status.model_sonnet,
        Tone::ModelOpus => palette.status.model_opus,
        Tone::ModelFable => palette.status.model_fable,
        Tone::StatusModel56 => palette.model_gpt56,
        Tone::StatusModelSol => palette.model_sol,
        Tone::StatusModelTerra => palette.model_terra,
        Tone::StatusModelLuna => palette.model_luna,
        Tone::StatusModelSpark => palette.model_spark,
        Tone::StatusModel55 => palette.model_gpt55,
        Tone::StatusModelHaiku => palette.status.model_haiku,
        Tone::StatusModelSonnet => palette.status.model_sonnet,
        Tone::StatusModelOpus => palette.status.model_opus,
        Tone::StatusModelFable => palette.status.model_fable,
        Tone::StatusEffortLow => palette.status.effort_low,
        Tone::StatusEffortMedium => palette.status.effort_medium,
        Tone::StatusEffortHigh => palette.status.effort_high,
        Tone::StatusEffortXHigh => palette.status.effort_xhigh,
        Tone::StatusEffortMax => palette.status.effort_max,
        Tone::StatusEffortUltra => palette.status.effort_ultra,
        Tone::Border => palette.border,
        Tone::SidePanelDivider => blend(
            palette.hover_bg,
            palette.foreground,
            SIDE_PANEL_DIVIDER_BLEND,
        ),
        Tone::Branch => palette.status.branch,
        Tone::LimitFiveHour => palette.status.five_hour,
        Tone::LimitWeekly => palette.status.weekly,
        Tone::FastOn => palette.blue,
        Tone::FastOff => palette.muted,
        Tone::VibeSuper => palette.warning,
        Tone::ClaudeAcceptEdits => theme::claude_mode_colors().accept_edits,
        Tone::ClaudePlan => theme::claude_mode_colors().plan,
        Tone::ClaudeAuto => theme::claude_mode_colors().auto,
        Tone::ClaudeBypass => theme::claude_mode_colors().bypass,
        Tone::ModelChange => palette.foreground,
        Tone::SyntaxComment => palette.syntax_comment,
        Tone::SyntaxString => palette.syntax_string,
        Tone::SyntaxKeyword => palette.syntax_keyword,
        Tone::SyntaxNumber => palette.syntax_number,
        Tone::SyntaxType => palette.syntax_type,
        Tone::SyntaxFunction => palette.syntax_function,
        Tone::SyntaxAttribute => palette.syntax_attribute,
        Tone::SyntaxProperty => palette.syntax_property,
        Tone::MarkdownHeading => palette.response.heading,
        Tone::MarkdownLink => palette.response.link,
        Tone::InlineCode => palette.response.inline_code,
        // Claude Code paints diff rows with the default text colour and lets the
        // green/red background carry the added/removed signal, so the text stays
        // as readable as the rest of the transcript.
        Tone::DiffAdded | Tone::DiffRemoved | Tone::DiffAddedWord | Tone::DiffRemovedWord => {
            palette.foreground
        }
        Tone::DiffHeader => palette.diff_header,
        Tone::Shimmer(base, level) => blend(base, palette.foreground, level),
        Tone::PlanShimmer(effort, level) => blend(palette.foreground, effort, level),
        Tone::ResponseTransition(base, level) => blend(palette.background, base, level),
        Tone::CopyJoin => return None,
    })
}

/// `level` of `0` is `from`, `255` is `to`, everything between is a straight
/// per-channel mix — enough for a shimmer that reads as one smooth gradient.
fn blend(from: Rgb, to: Rgb, level: u8) -> Rgb {
    let mix = |from: u8, to: u8| {
        let from = f32::from(from);
        (from + (f32::from(to) - from) * f32::from(level) / 255.0).round() as u8
    };
    Rgb(mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

fn set_tone(out: &mut impl Write, tone: Tone) -> Result<()> {
    queue!(
        out,
        SetForegroundColor(tone_rgb(tone).map_or(Color::Reset, rgb_color))
    )?;
    if tone == Tone::Thinking {
        queue!(out, SetAttribute(Attribute::Italic))?;
    }
    if tone == Tone::MarkdownLink {
        queue!(out, SetAttribute(Attribute::Underlined))?;
    }
    if tone == Tone::PlanDone {
        queue!(out, SetAttribute(Attribute::CrossedOut))?;
    }
    Ok(())
}

fn rgb_color(color: Rgb) -> Color {
    Color::Rgb {
        r: color.0,
        g: color.1,
        b: color.2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel takes a fixed slice of the right edge, leaves a gap in front of
    /// it, and fills the terminal's last cell without printing into it directly.
    #[test]
    fn side_panel_layout_keeps_a_gap_and_reaches_the_right_edge() {
        let layout =
            side_panel_layout(100, SIDE_PANEL_WIDTHS[0]).expect("100 columns carry the panel");

        assert_eq!(layout.panel_width, SIDE_PANEL_WIDTHS[0]);
        assert_eq!(layout.panel_left, layout.main_width + SIDE_PANEL_GAP);
        assert_eq!(layout.panel_left + layout.panel_width, 100);
        assert_eq!(layout.content_left(), layout.panel_left + 2);
        assert_eq!(layout.content_width(), SIDE_PANEL_WIDTHS[0] - 4);
    }

    /// A terminal that cannot spare the room keeps the conversation full width
    /// rather than squeezing it behind the panel.
    #[test]
    fn a_narrow_terminal_refuses_to_open_the_side_panel() {
        let smallest = (SIDE_PANEL_MIN_MAIN_WIDTH + SIDE_PANEL_GAP + SIDE_PANEL_WIDTHS[0]) as u16;

        assert!(side_panel_layout(smallest - 1, SIDE_PANEL_WIDTHS[0]).is_none());
        assert_eq!(
            side_panel_layout(smallest, SIDE_PANEL_WIDTHS[0]).map(|layout| layout.main_width),
            Some(SIDE_PANEL_MIN_MAIN_WIDTH)
        );
    }

    /// A full-width transcript row stops before the gap, while every panel cell
    /// carries the theme's flat surface colour without border glyphs.
    #[test]
    fn a_bounded_row_leaves_the_gap_clear_and_fills_the_side_panel() {
        theme::set_current(ThemeKind::Dark);
        let layout =
            side_panel_layout(100, SIDE_PANEL_WIDTHS[0]).expect("100 columns carry the panel");
        let mut frame = CellFrame::new(100, 3);
        let line = PaintLine {
            tone: Tone::UserPromptPadding,
            ..PaintLine::plain("hello")
        };

        paint_line_into_frame(&mut frame, 0, &line, None, None, Some(layout.main_width));
        paint_side_panel_into_frame(&mut frame, layout, 3, &[], None);

        assert_eq!(frame.cell(layout.main_width, 0).style, CellStyle::plain());
        let background = Some(theme::palette().hover_bg);
        assert_eq!(
            frame.cell(layout.panel_left, 0).style.background,
            background
        );
        let right = layout.panel_left + layout.panel_width - 1;
        assert_eq!(frame.cell(right, 1).style.background, background);
        assert_eq!(frame.cell(99, 1).style.background, background);
        for row in 0..3 {
            assert_eq!(frame.cell(layout.panel_left, row).glyph, " ");
            assert_eq!(frame.cell(right, row).glyph, " ");
        }
    }

    /// The top inset is part of the same flat surface and contains no rule,
    /// corner, caption, or shortcut hint.
    #[test]
    fn the_side_panel_top_row_is_empty_background() {
        let layout =
            side_panel_layout(100, SIDE_PANEL_WIDTHS[0]).expect("100 columns carry the panel");
        let mut frame = CellFrame::new(100, 3);

        paint_side_panel_into_frame(&mut frame, layout, 3, &[], None);

        for column in layout.panel_left..layout.panel_left + layout.panel_width {
            let cell = frame.cell(column, 0);
            assert_eq!(cell.glyph, " ");
            assert_eq!(cell.style.background, Some(theme::palette().hover_bg));
            assert_eq!(cell.style.foreground, None);
        }
    }

    #[test]
    fn the_side_panel_places_a_subdued_divider_above_context() {
        let layout =
            side_panel_layout(100, SIDE_PANEL_WIDTHS[0]).expect("100 columns carry the panel");
        let mut frame = CellFrame::new(100, 4);
        let footer = vec![
            side_panel_divider(layout.content_width()),
            PaintLine::plain("Context"),
        ];

        paint_side_panel_into_frame_with_footer(&mut frame, layout, 4, &[], None, &footer, None);

        let context: String = (0..UnicodeWidthStr::width("Context"))
            .map(|offset| frame.cell(layout.content_left() + offset, 2).glyph.clone())
            .collect();
        assert_eq!(context, "Context");
        for column in layout.panel_left..layout.panel_left + layout.panel_width {
            let cell = frame.cell(column, 1);
            let in_content = (layout.content_left()
                ..layout.content_left() + layout.content_width())
                .contains(&column);
            assert_eq!(cell.glyph, if in_content { "─" } else { " " });
            assert_eq!(cell.style.background, Some(theme::palette().hover_bg));
            assert_eq!(
                cell.style.foreground,
                in_content.then(|| tone_rgb(Tone::SidePanelDivider).unwrap())
            );
        }
    }

    /// Every theme supplies a readable secondary surface, and the docked panel
    /// uses it from edge to edge without foreground border cells.
    #[test]
    fn the_side_panel_uses_each_themes_secondary_surface() {
        let layout =
            side_panel_layout(100, SIDE_PANEL_WIDTHS[0]).expect("100 columns carry the panel");

        for theme in ThemeKind::ALL {
            theme::set_current(theme);
            let mut frame = CellFrame::new(100, 3);
            paint_side_panel_into_frame(&mut frame, layout, 3, &[], None);

            let edge = frame.cell(layout.panel_left, 0);
            assert_eq!(edge.glyph, " ");
            assert_eq!(edge.style.foreground, None);
            assert_eq!(edge.style.background, Some(theme::palette().hover_bg));
        }
        theme::set_current(ThemeKind::Dark);
    }

    #[test]
    fn side_panel_dividers_are_softer_than_the_old_border_tone() {
        let distance = |left: Rgb, right: Rgb| {
            u16::from(left.0.abs_diff(right.0))
                + u16::from(left.1.abs_diff(right.1))
                + u16::from(left.2.abs_diff(right.2))
        };

        for theme in ThemeKind::ALL {
            theme::set_current(theme);
            let palette = theme::palette();
            let divider = tone_rgb(Tone::SidePanelDivider).expect("divider color");
            assert!(
                distance(palette.hover_bg, divider) < distance(palette.hover_bg, palette.border)
            );
        }
        theme::set_current(ThemeKind::Dark);
    }

    /// An empty panel is one uninterrupted background rectangle.
    #[test]
    fn the_empty_side_panel_carries_only_its_background() {
        let layout =
            side_panel_layout(100, SIDE_PANEL_WIDTHS[0]).expect("100 columns carry the panel");
        let mut frame = CellFrame::new(100, 4);

        paint_side_panel_into_frame(&mut frame, layout, 4, &[], None);

        for row in 0..4 {
            for column in layout.panel_left..layout.panel_left + layout.panel_width {
                let cell = frame.cell(column, row);
                assert_eq!(cell.glyph, " ");
                assert_eq!(cell.style.background, Some(theme::palette().hover_bg));
            }
        }
    }

    /// Repainting one animated row restores the same flat panel surface.
    #[test]
    fn a_single_row_repaint_redraws_that_row_of_the_side_panel() {
        let layout =
            side_panel_layout(100, SIDE_PANEL_WIDTHS[0]).expect("100 columns carry the panel");
        let mut row = CellFrame::new(100, 1);

        paint_line_into_frame(
            &mut row,
            0,
            &PaintLine::plain("working"),
            None,
            None,
            Some(layout.main_width),
        );
        paint_side_panel_row_into_frame(&mut row, layout, 0, 1, 30, &[], None, &[], None);

        assert_eq!(
            row.cell(layout.panel_left, 0).style.background,
            Some(theme::palette().hover_bg)
        );
        assert_eq!(row.cell(layout.panel_left, 0).glyph, " ");
        let right = layout.panel_left + layout.panel_width - 1;
        assert_eq!(row.cell(right, 0).glyph, " ");
        assert_eq!(
            row.cell(right, 0).style.background,
            Some(theme::palette().hover_bg)
        );
        assert_eq!(row.cell(layout.content_left(), 0).glyph, " ");
    }

    #[test]
    fn word_selection_uses_word_boundaries_and_content_columns() {
        let line = CopyLine {
            text: "│ hello_world!".to_owned(),
            join_next: false,
            marker_width: 0,
            prefix_width: 0,
            content_columns: Some(2..14),
        };

        assert_eq!(word_range_at(&line, 7), Some(2..13));
        assert_eq!(word_range_at(&line, 13), None);
        assert_eq!(word_range_at(&line, 0), None);
    }

    #[test]
    fn frame_clips_glyphs_before_the_autowrap_column() {
        let mut frame = CellFrame::new(8, 1);

        frame.write(6, 0, "xy", CellStyle::plain());

        assert_eq!(frame.cell(6, 0).glyph, "x");
        assert_eq!(frame.cell(7, 0).glyph, " ");
    }

    #[test]
    fn user_prompt_background_leaves_the_rightmost_cell_unpainted() {
        CHAT_LAYOUT.store(false, Ordering::Relaxed);
        let mut frame = CellFrame::new(8, 1);

        paint_line_into_frame(
            &mut frame,
            0,
            &PaintLine {
                prefix: " ".to_owned(),
                prefix_tone: Tone::Plain,
                text: "prompt".to_owned(),
                tone: Tone::UserPrompt,
                bold: true,
                tool_heading: None,
                pick: None,
                tail: Vec::new(),
            },
            None,
            None,
            None,
        );

        assert_eq!(
            frame.cell(6, 0).style.background,
            row_background(Tone::UserPrompt)
        );
        assert_eq!(frame.cell(7, 0).style.background, None);
    }

    #[test]
    fn user_prompt_border_uses_the_prompt_background() {
        CHAT_LAYOUT.store(false, Ordering::Relaxed);
        let mut frame = CellFrame::new(8, 1);

        paint_line_into_frame(
            &mut frame,
            0,
            &PaintLine {
                prefix: "▌ ".to_owned(),
                prefix_tone: Tone::Accent,
                text: "x".to_owned(),
                tone: Tone::UserPrompt,
                bold: false,
                tool_heading: None,
                pick: None,
                tail: Vec::new(),
            },
            None,
            None,
            None,
        );

        assert_eq!(
            frame.cell(0, 0).style.background,
            row_background(Tone::UserPrompt)
        );
        assert_eq!(
            frame.cell(1, 0).style.background,
            row_background(Tone::UserPrompt)
        );
        assert_eq!(
            frame.cell(2, 0).style.background,
            row_background(Tone::UserPrompt)
        );
        assert_eq!(
            frame.cell(6, 0).style.background,
            row_background(Tone::UserPrompt)
        );
        assert_eq!(frame.cell(7, 0).style.background, None);
    }

    #[test]
    fn wrapped_user_prompt_repeats_its_border_on_every_row() {
        CHAT_LAYOUT.store(false, Ordering::Relaxed);
        let width = 18;
        let lines = user_prompt_lines(
            &Block::new(
                BlockKind::User,
                "You",
                "a prompt long enough to wrap across several rows",
            ),
            width,
        );

        let prompt_rows = lines
            .iter()
            .filter(|line| line.tone == Tone::UserPrompt)
            .collect::<Vec<_>>();
        assert!(prompt_rows.len() > 1);
        assert!(prompt_rows.iter().all(|line| line.prefix == "▌ "));
        assert!(
            prompt_rows
                .iter()
                .all(|line| painted_line_width(line) <= usize::from(width).saturating_sub(2))
        );
    }

    #[test]
    fn model_change_background_leaves_the_same_rightmost_cell_as_user_prompt() {
        let mut frame = CellFrame::new(8, 1);

        paint_line_into_frame(
            &mut frame,
            0,
            &PaintLine {
                prefix: "  ".to_owned(),
                prefix_tone: Tone::ModelChange,
                text: "Fast mode On".to_owned(),
                tone: Tone::ModelChange,
                bold: true,
                tool_heading: None,
                pick: None,
                tail: Vec::new(),
            },
            None,
            None,
            None,
        );

        assert_eq!(
            frame.cell(6, 0).style.background,
            row_background(Tone::ModelChange)
        );
        assert_eq!(frame.cell(7, 0).style.background, None);
    }

    #[test]
    fn terminal_diff_coalesces_adjacent_changed_cells_with_the_same_style() {
        let previous = CellFrame::new(8, 1);
        let mut current = previous.clone();
        current.write(0, 0, "abc", CellStyle::plain());

        let mut output = Vec::new();
        emit_frame_diff(&mut output, Some(&previous), &current).expect("frame diff emits");

        let output = String::from_utf8(output).expect("terminal bytes are UTF-8");
        assert_eq!(output.matches("\x1b[1;1H").count(), 1);
        assert!(output.contains("abc"));
    }

    #[test]
    fn terminal_diff_starts_a_new_run_when_the_style_changes() {
        let previous = CellFrame::new(8, 1);
        let mut current = previous.clone();
        current.write(0, 0, "a", CellStyle::plain());
        current.write(
            1,
            0,
            "b",
            CellStyle {
                bold: true,
                ..CellStyle::plain()
            },
        );

        let mut output = Vec::new();
        emit_frame_diff(&mut output, Some(&previous), &current).expect("frame diff emits");

        assert_eq!(
            String::from_utf8(output)
                .expect("terminal bytes are UTF-8")
                .matches("\x1b[1;")
                .count(),
            2
        );
    }

    #[test]
    fn synchronized_frame_diff_brackets_the_complete_diff() {
        let previous = CellFrame::new(8, 1);
        let mut current = previous.clone();
        current.write(0, 0, "x", CellStyle::plain());

        let mut output = Vec::new();
        emit_synchronized_frame_diff_with_full_rows(
            &mut output,
            Some(&previous),
            &current,
            &[],
            false,
            None,
            true,
        )
        .expect("synchronized frame diff emits");

        let output = String::from_utf8(output).expect("terminal bytes are UTF-8");
        assert!(output.starts_with("\x1b[?2026h"));
        assert!(output.contains('x'));
        assert!(output.ends_with("\x1b[?2026l"));
    }

    #[test]
    fn animation_row_diff_targets_its_screen_row() {
        let previous = CellFrame::new(8, 1);
        let mut current = previous.clone();
        current.write(0, 0, "x", CellStyle::plain());
        let mut output = Vec::new();

        emit_frame_diff_at(&mut output, Some(&previous), &current, 7).expect("row diff emits");

        let output = String::from_utf8(output).expect("terminal bytes are UTF-8");
        assert!(output.contains("\x1b[8;1H"));
        assert!(output.contains('x'));
    }

    #[test]
    fn changed_korean_text_uses_a_local_safe_repaint_range() {
        let mut before = CellFrame::new(16, 1);
        before.write(0, 0, "한글 단어", CellStyle::plain());
        let mut after = CellFrame::new(16, 1);
        after.write(0, 0, "한글 ", CellStyle::plain());

        let damage = wide_damage_range(&before, &after, 0).expect("wide text changed");
        assert_eq!(damage, 5..9);
    }

    #[test]
    fn changed_korean_text_does_not_clear_or_repaint_the_whole_row() {
        let background = Rgb(1, 2, 3);
        let style = CellStyle {
            background: Some(background),
            ..CellStyle::plain()
        };
        let mut before = CellFrame::new(16, 1);
        before.fill(0, 0, 16, 1, style);
        before.write(4, 0, "한글 단어", style);
        let mut after = CellFrame::new(16, 1);
        after.fill(0, 0, 16, 1, style);
        after.write(4, 0, "한글 새", style);

        let mut output = Vec::new();
        emit_frame_diff(&mut output, Some(&before), &after).expect("frame diff emits");

        let output = String::from_utf8(output).expect("terminal bytes are UTF-8");
        assert!(!output.contains("\x1b[2K"));
        assert!(!output.contains("\x1b[1;1H"));
        assert!(output.contains("새"));
    }

    #[test]
    fn korean_text_shift_repaints_from_a_leading_cell_not_a_continuation() {
        let mut before = CellFrame::new(12, 1);
        before.write(4, 0, "한", CellStyle::plain());
        let mut after = CellFrame::new(12, 1);
        after.write(4, 0, "x한", CellStyle::plain());

        assert_eq!(wide_damage_range(&before, &after, 0), Some(4..7));

        let mut output = Vec::new();
        emit_frame_diff(&mut output, Some(&before), &after).expect("frame diff emits");
        let output = String::from_utf8(output).expect("terminal bytes are UTF-8");

        assert!(output.contains("\x1b[1;5H"));
        assert!(output.contains("x한"));
        assert!(!output.contains("\x1b[1;6H"));
        assert!(!output.contains("\x1b[2K"));
    }

    #[test]
    fn synchronized_frame_diff_ends_the_bracket_after_a_paint_error() {
        struct FailsOnGlyph {
            bytes: Vec<u8>,
            failed: bool,
        }

        impl Write for FailsOnGlyph {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if !self.failed && bytes == b"x" {
                    self.failed = true;
                    return Err(std::io::Error::other("test write failure"));
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let previous = CellFrame::new(8, 1);
        let mut current = previous.clone();
        current.write(0, 0, "x", CellStyle::plain());
        let mut output = FailsOnGlyph {
            bytes: Vec::new(),
            failed: false,
        };

        assert!(
            emit_synchronized_frame_diff_with_full_rows(
                &mut output,
                Some(&previous),
                &current,
                &[],
                false,
                None,
                false,
            )
            .is_err()
        );
        assert!(
            String::from_utf8(output.bytes)
                .expect("terminal bytes are UTF-8")
                .ends_with("\x1b[?2026l")
        );
    }

    #[test]
    fn terminal_diff_uses_erase_for_a_changed_final_cell() {
        let previous = CellFrame::new(8, 1);
        let mut current = previous.clone();
        current.fill(
            7,
            0,
            8,
            1,
            CellStyle {
                background: Some(theme::palette().border),
                ..CellStyle::plain()
            },
        );
        let mut output = Vec::new();

        emit_frame_diff(&mut output, Some(&previous), &current).expect("frame diff emits");

        assert!(
            String::from_utf8(output)
                .expect("terminal bytes are UTF-8")
                .contains("\x1b[K")
        );
    }

    #[test]
    fn fullscreen_entry_preserves_alternate_scroll_order() {
        let mut output = Vec::new();

        enter_fullscreen(&mut output).expect("fullscreen command");

        assert_eq!(
            output, b"\x1b[?1049h\x1b[?1007s\x1b[?1007l",
            "enter alternate screen, save alternate-scroll mode, then disable it"
        );
    }

    #[test]
    fn mouse_capture_can_reassert_disabled_alternate_scroll() {
        let mut output = Vec::new();

        disable_alternate_scroll(&mut output).expect("alternate-scroll disable command");

        assert_eq!(output, b"\x1b[?1007l");
    }

    #[test]
    fn fullscreen_exit_restores_alternate_scroll_before_leaving() {
        let mut output = Vec::new();

        leave_fullscreen(&mut output).expect("fullscreen exit command");

        assert_eq!(output, b"\x1b[?1007r\x1b[?1049l");
    }

    #[test]
    fn a_selection_paints_its_theme_block() {
        // The block has to be read while this theme is still current: the theme
        // is global and the test suite runs in parallel, so a sibling test can
        // flip it out from under us between the paint and the assertion.
        let selected_bytes = |kind| {
            theme::set_current(kind);
            let mut output = Vec::new();
            print_line_with_selection(&mut output, &PaintLine::plain("abcdef"), Some(1..3), None)
                .expect("selection paint");
            let bg = theme::selection_bg();
            (
                String::from_utf8(output).expect("utf-8 escape sequences"),
                bg,
            )
        };

        for kind in ThemeKind::ALL {
            let (painted, bg) = selected_bytes(kind);
            let block = format!("\x1b[48;2;{};{};{}m", bg.0, bg.1, bg.2);
            let selected = painted
                .split_once(&block)
                .map(|(_, rest)| rest)
                .unwrap_or_else(|| panic!("{kind:?} must paint its own block: {painted:?}"));
            assert!(
                selected.contains("bc"),
                "{kind:?} must highlight exactly the dragged cells: {painted:?}"
            );
            assert!(
                !painted.contains("\x1b[7m"),
                "{kind:?} must not fall back to reverse video: {painted:?}"
            );
        }
        theme::set_current(ThemeKind::Dark);
    }

    #[test]
    fn bash_hover_background_starts_after_the_disclosure_arrow() {
        theme::set_current(ThemeKind::Dark);
        let block = Block::new(BlockKind::Tool, "Shell · cargo test", "");
        let line = block_lines(&block, 80).remove(0);
        let mut output = Vec::new();

        let hovered = Renderer::hover_columns(&line, line.tool_heading, None);
        print_line_with_selection(&mut output, &line, None, hovered).expect("hover paint");
        let painted = String::from_utf8(output).expect("utf-8 escapes");
        let hover = theme::palette().hover_bg;
        let hover_escape = format!("\x1b[48;2;{};{};{}m", hover.0, hover.1, hover.2);

        let arrow = painted.find("▸ ").expect("arrow");
        let background = painted.find(&hover_escape).expect("hover background");
        let title = painted.find("Shell").expect("title");
        assert!(arrow < background);
        assert!(background < title);
    }

    #[test]
    fn selected_text_stays_legible_on_every_theme_and_tone() {
        for kind in ThemeKind::ALL {
            theme::set_current(kind);
            for tone in [
                Tone::Plain,
                Tone::Muted,
                Tone::Accent,
                Tone::Success,
                Tone::Error,
                Tone::SyntaxKeyword,
                Tone::SyntaxComment,
                Tone::DiffAdded,
                Tone::DiffRemoved,
            ] {
                let color = tone_rgb(tone).expect("a printable tone has a colour");
                let painted = theme::selection_text(color);
                assert!(
                    theme::contrast_ratio(painted, theme::selection_bg()) >= 4.5,
                    "{kind:?}/{tone:?} is unreadable inside the selection block"
                );
            }
        }
        theme::set_current(ThemeKind::Dark);
    }

    #[test]
    fn a_selection_keeps_colours_the_block_can_carry() {
        theme::set_current(ThemeKind::Dark);
        let palette = theme::palette();

        // Dark's own foreground is bright enough to survive the blue wash,
        // so the drag must not flatten it the way reverse video would.
        assert_eq!(
            theme::selection_text(palette.foreground),
            palette.foreground
        );
        assert_eq!(
            theme::selection_text(theme::MINIMAL.foreground),
            theme::selection_fg(),
            "near-black light-theme text has to step aside for the fallback"
        );
    }

    #[test]
    fn each_theme_tints_its_selection_differently() {
        // Read straight off the palettes, not through the global current theme:
        // sibling tests flip that while this one runs.
        let blocks = [
            theme::MINIMAL.selection_bg,
            theme::SOFT.selection_bg,
            theme::DARK.selection_bg,
        ];

        for (index, block) in blocks.iter().enumerate() {
            for other in &blocks[index + 1..] {
                assert_ne!(block, other, "two themes share one selection tone");
            }
        }
    }

    #[test]
    fn docking_adds_space_before_the_composer() {
        let mut frame = Frame {
            lines: vec![
                PaintLine::plain("response"),
                PaintLine::plain("input"),
                PaintLine::plain("footer"),
            ],
            cursor_line: 1,
            cursor_col: 0,
            show_cursor: true,
            dock_index: 1,
            composer_index: Some(1),
            composer_layout: None,
            activity_index: None,
        };

        fit_frame(&mut frame, 6);

        assert_eq!(frame.lines.len(), 6);
        assert_eq!(frame.cursor_line, 4);
        assert_eq!(frame.lines[0].text, "response");
        assert_eq!(frame.lines[4].text, "input");
        assert_eq!(frame.lines[5].text, "footer");
    }

    #[test]
    fn docking_keeps_the_activity_row_address_in_sync() {
        let mut frame = Frame {
            lines: vec![
                PaintLine::plain("response"),
                PaintLine::plain("working"),
                PaintLine::plain("input"),
            ],
            cursor_line: 2,
            cursor_col: 0,
            show_cursor: true,
            dock_index: 1,
            composer_index: Some(2),
            composer_layout: None,
            activity_index: Some(1),
        };

        fit_frame(&mut frame, 6);

        assert_eq!(frame.activity_index, Some(4));
        assert_eq!(frame.lines[4].text, "working");
    }

    #[test]
    fn docking_trims_oldest_rows_before_the_composer() {
        let mut frame = Frame {
            lines: (0..5)
                .map(|index| PaintLine::plain(index.to_string()))
                .collect(),
            cursor_line: 3,
            cursor_col: 0,
            show_cursor: true,
            dock_index: 3,
            composer_index: Some(3),
            composer_layout: None,
            activity_index: None,
        };

        fit_frame(&mut frame, 3);

        assert_eq!(
            frame
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["2", "3", "4"]
        );
        assert_eq!(frame.cursor_line, 1);
    }

    #[test]
    fn composer_rows_render_as_a_closed_box() {
        let mut editor = Editor::default();
        editor.set_text("wrapped-prompt-text");

        let (rows, _, _, _) = input_lines(&editor, &[], 18, "", "placeholder", None, None);
        let prompt_rows = &rows[1..rows.len() - 1];

        assert!(prompt_rows.len() > 1);
        assert!(!rows[0].text.contains("Message"));
        assert_eq!(painted(&rows[0]), "╭───────────────╮");
        // Both rules are drawn in the same border colour the welcome card uses.
        assert!(rows[0].tone == Tone::Border);
        assert!(rows.last().is_some_and(|row| row.tone == Tone::Border));
        // The IME gutter keeps four columns clear before the right border, so
        // the text wraps that much earlier.
        assert_eq!(painted(&prompt_rows[0]), "│ > wrapped-    │");
        assert_eq!(painted(&prompt_rows[1]), "│   prompt-t    │");
        assert_eq!(
            painted(rows.last().expect("bottom rule")),
            "╰───────────────╯"
        );
    }

    #[test]
    fn vertical_cursor_navigation_uses_wrapped_composer_rows_before_their_edges() {
        let mut editor = Editor::default();
        editor.set_text("abcdefghijkl");
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.set_composer_navigation_layout_for_test(&editor, 18);

        assert_eq!(renderer.composer_vertical_cursor_position(12, -1), Some(4));
        assert_eq!(renderer.composer_vertical_cursor_position(4, 1), Some(12));
        assert_eq!(renderer.composer_vertical_cursor_position(4, -1), None);
        assert_eq!(renderer.composer_vertical_cursor_position(12, 1), None);
    }

    #[test]
    fn a_tall_draft_stops_growing_and_scrolls_with_the_cursor() {
        let mut editor = Editor::default();
        editor.set_text(
            (1..=20)
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );

        let (rows, cursor_row, _, layout) =
            input_lines(&editor, &[], 40, "", "placeholder", None, None);

        assert_eq!(
            rows.len(),
            COMPOSER_MAX_PROMPT_ROWS + 2,
            "the two rules stay"
        );
        assert_eq!(layout.rows.len(), COMPOSER_MAX_PROMPT_ROWS);
        assert_eq!(
            cursor_row, COMPOSER_MAX_PROMPT_ROWS,
            "the last row is in view"
        );
        assert!(
            painted(&rows[1]).contains("11"),
            "the window follows the cursor"
        );
        assert!(painted(&rows[COMPOSER_MAX_PROMPT_ROWS]).contains("20"));

        // Back at the top, the window shows the first rows and the prompt mark.
        for _ in 0..19 {
            editor.move_up();
        }
        let (rows, cursor_row, _, _) = input_lines(&editor, &[], 40, "", "placeholder", None, None);
        assert_eq!(cursor_row, 1);
        assert!(painted(&rows[1]).starts_with("│ > 1"));
        assert!(painted(&rows[COMPOSER_MAX_PROMPT_ROWS]).contains("10"));
    }

    #[test]
    fn korean_composer_text_keeps_an_ime_gutter_before_the_right_border() {
        let mut editor = Editor::default();
        editor.set_text("가가가가가가");

        let (rows, cursor_row, cursor_col, _) =
            input_lines(&editor, &[], 18, "", "placeholder", None, None);

        assert_eq!((cursor_row, cursor_col), (2, 8));
        assert!(painted(&rows[1]).contains("가가가가"));
        assert!(painted(&rows[2]).contains("가가"));
        assert!(rows.iter().all(|row| painted_width(row) == 17));

        editor.set_text("가가가가가가나");
        let (rows, cursor_row, cursor_col, _) =
            input_lines(&editor, &[], 18, "", "placeholder", None, None);

        assert_eq!((cursor_row, cursor_col), (2, 10));
        assert!(painted(&rows[1]).contains("가가가가"));
        assert!(painted(&rows[2]).contains("가가나"));
        assert!(rows.iter().all(|row| painted_width(row) == 17));
    }

    #[test]
    fn collapsed_paste_cursor_wraps_before_the_right_edge() {
        let mut editor = Editor::default();
        editor.insert_paste_str("1\n2\n3\n4\n5\n6");
        editor.insert_str("abc");
        editor.move_home();
        editor.move_right();

        let (_, display_cursor) = composer_display(&editor, &[]);
        assert_eq!(display_cursor, 24);

        let (_, cursor_row, cursor_col, _) =
            input_lines(&editor, &[], 18, "", "placeholder", None, None);

        assert_eq!((cursor_row, cursor_col), (4, 4));
    }

    #[test]
    fn fullscreen_composer_copy_excludes_the_box_chrome() {
        let mut editor = Editor::default();
        editor.set_text("copy");
        let (rows, _, _, _) = input_lines(&editor, &[], 18, "", "placeholder", None, None);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = rows;

        assert!(renderer.begin_selection(0, 1));
        assert!(renderer.update_selection(16, 1));
        assert_eq!(
            renderer.finish_selection(16, 1),
            SelectionResult::Copy("copy".to_owned())
        );
    }

    /// The transcript and the docked panel are two separate columns of text, so
    /// a drag copies the one it started on and never mixes the two.
    #[test]
    fn a_drag_inside_the_side_panel_copies_the_panel_not_the_transcript() {
        let layout =
            side_panel_layout(100, SIDE_PANEL_WIDTHS[0]).expect("100 columns carry the panel");
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = vec![
            PaintLine::plain("transcript row"),
            PaintLine::plain("second row"),
        ];
        renderer.side_panel = Some(layout);
        renderer.side_panel_content = vec![
            PaintLine::plain("Updated Plan  1 / 1"),
            PaintLine::blank(),
            PaintLine::plain("panel step"),
        ];

        // Row 0 is the panel's top rule, so its third content row is screen row 3.
        let start = layout.content_left() as u16;
        assert!(renderer.begin_selection(start, 3));
        assert!(renderer.update_selection(start + 9, 3));
        assert_eq!(
            renderer.finish_selection(start + 9, 3),
            SelectionResult::Copy("panel step".to_owned())
        );

        // A drag that starts in the transcript still answers with transcript text.
        assert!(renderer.begin_selection(0, 0));
        assert!(renderer.update_selection(13, 0));
        assert_eq!(
            renderer.finish_selection(13, 0),
            SelectionResult::Copy("transcript row".to_owned())
        );
    }

    /// A drag that runs off the bottom of the panel has to land on its last
    /// content row. Letting the row index reach the content length instead
    /// drops the point entirely and the drag stops answering mid-gesture.
    #[test]
    fn a_panel_drag_past_the_last_row_still_lands_on_it() {
        let layout =
            side_panel_layout(100, SIDE_PANEL_WIDTHS[0]).expect("100 columns carry the panel");
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = vec![PaintLine::plain("transcript row")];
        renderer.side_panel = Some(layout);
        renderer.side_panel_content = vec![
            PaintLine::plain("first"),
            PaintLine::blank(),
            PaintLine::plain("last"),
        ];

        let left = layout.content_left() as u16;
        assert!(renderer.begin_selection(left, 1));
        assert!(renderer.update_selection(left + 20, 40));
        assert_eq!(
            renderer.finish_selection(left + 20, 40),
            SelectionResult::Copy("first\n\nlast".to_owned())
        );
    }

    #[test]
    fn a_drag_over_the_composer_answers_in_characters() {
        let mut editor = Editor::default();
        editor.set_text("alpha beta");
        let (rows, _, _, layout) = input_lines(&editor, &[], 40, "", "placeholder", None, None);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = rows;
        renderer.composer_selection = Some(ComposerSelection {
            first_row: 1,
            layout,
        });

        // The prompt text starts at column 4, so this drag covers "beta".
        assert!(renderer.begin_selection(10, 1));
        assert!(renderer.update_selection(13, 1));

        assert_eq!(renderer.composer_selection_range(), Some(6..10));

        // Chrome on its own selects nothing to delete.
        assert!(renderer.begin_selection(0, 1));
        assert_eq!(renderer.composer_selection_range(), None);
    }

    #[test]
    fn composer_clicks_resolve_text_width_and_line_boundaries() {
        let mut editor = Editor::default();
        editor.set_text("ab한\ncd");
        let (rows, _, _, layout) = input_lines(&editor, &[], 40, "", "placeholder", None, None);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = rows;
        renderer.composer_selection = Some(ComposerSelection {
            first_row: 1,
            layout,
        });

        assert_eq!(renderer.composer_cursor_position(0, 1), Some(0));
        assert_eq!(renderer.composer_cursor_position(5, 1), Some(1));
        assert_eq!(renderer.composer_cursor_position(6, 1), Some(2));
        assert_eq!(renderer.composer_cursor_position(7, 1), Some(3));
        assert_eq!(renderer.composer_cursor_position(30, 1), Some(3));
        assert_eq!(renderer.composer_cursor_position(0, 2), Some(4));
        assert_eq!(renderer.composer_cursor_position(30, 2), Some(6));
        assert_eq!(renderer.composer_cursor_position(4, 0), None);
        assert_eq!(renderer.composer_cursor_position(4, 3), None);
    }

    #[test]
    fn composer_clicks_clamp_to_visual_wrap_boundaries() {
        let mut editor = Editor::default();
        editor.set_text("abcdefgh");
        let (rows, _, _, layout) = input_lines(&editor, &[], 16, "", "placeholder", None, None);
        assert_eq!(layout.rows.len(), 2);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = rows;
        renderer.composer_selection = Some(ComposerSelection {
            first_row: 1,
            layout,
        });

        assert_eq!(renderer.composer_cursor_position(14, 1), Some(7));
        assert_eq!(renderer.composer_cursor_position(0, 2), Some(7));
        assert_eq!(renderer.composer_cursor_position(14, 2), Some(8));
    }

    #[test]
    fn composer_clicks_treat_an_image_label_as_one_character() {
        let mut editor = Editor::default();
        editor.set_text("a");
        editor.insert_attachment();
        editor.insert_str("b");
        let (rows, _, _, layout) = input_lines(
            &editor,
            &["image.png".to_owned()],
            40,
            "",
            "placeholder",
            None,
            None,
        );
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = rows;
        renderer.composer_selection = Some(ComposerSelection {
            first_row: 1,
            layout,
        });

        assert_eq!(renderer.composer_cursor_position(6, 1), Some(1));
        assert_eq!(renderer.composer_cursor_position(16, 1), Some(2));
    }

    #[test]
    fn composer_clicks_use_the_visible_window_of_a_long_draft() {
        let mut editor = Editor::default();
        editor.set_text(
            (0..12)
                .map(|index| format!("row{index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let (rows, _, _, layout) = input_lines(&editor, &[], 40, "", "placeholder", None, None);
        assert_eq!(layout.rows.len(), COMPOSER_MAX_PROMPT_ROWS);
        let first_visible = "row0\nrow1\n".chars().count();
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = rows;
        renderer.composer_selection = Some(ComposerSelection {
            first_row: 1,
            layout,
        });

        assert_eq!(renderer.composer_cursor_position(0, 1), Some(first_visible));
        assert_eq!(
            renderer.composer_cursor_position(30, 10),
            Some(editor.chars().len())
        );
    }

    #[test]
    fn a_drag_across_composer_rows_takes_the_line_break_with_it() {
        let mut editor = Editor::default();
        editor.set_text("one\ntwo");
        let (rows, _, _, layout) = input_lines(&editor, &[], 40, "", "placeholder", None, None);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = rows;
        renderer.composer_selection = Some(ComposerSelection {
            first_row: 1,
            layout,
        });

        assert!(renderer.begin_selection(5, 1));
        assert!(renderer.update_selection(4, 2));

        // "ne" plus the break plus "t": the newline sits between the two rows.
        assert_eq!(renderer.composer_selection_range(), Some(1..5));
    }

    #[test]
    fn a_drag_over_a_collapsed_paste_answers_for_the_whole_block() {
        let mut editor = Editor::default();
        editor.insert_paste_str("one\ntwo\nthree\nfour\nfive\nsix");
        let paste = editor
            .collapsed_paste_range()
            .expect("the paste stays collapsed");
        let (rows, _, _, layout) = input_lines(&editor, &[], 40, "", "placeholder", None, None);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = rows;
        renderer.composer_selection = Some(ComposerSelection {
            first_row: 1,
            layout,
        });

        // One cell of the summary is enough: it stands for the pasted block.
        assert!(renderer.begin_selection(6, 1));

        assert_eq!(renderer.composer_selection_range(), Some(paste));
    }

    #[test]
    fn fullscreen_composer_highlight_excludes_the_box_chrome() {
        let mut editor = Editor::default();
        editor.set_text("copy");
        let (rows, _, _, _) = input_lines(&editor, &[], 18, "", "placeholder", None, None);
        let range = CellRange {
            start: CellPosition { column: 0, row: 1 },
            end: CellPosition { column: 16, row: 1 },
        };

        assert_eq!(selection_columns_for_line(&rows[1], range, 1), Some(4..8));
    }

    /// Three releases of rule wording did not stop the English opener, so the
    /// display cuts it. Only a bare connective in front of Hangul goes.
    #[test]
    fn an_english_opener_in_front_of_hangul_is_cut_from_the_answer() {
        assert_eq!(
            without_leading_english_filler("Now 브리지에 상태 조회를 추가합니다."),
            "브리지에 상태 조회를 추가합니다."
        );
        assert_eq!(
            without_leading_english_filler("Alright, 토글 함수를 넣습니다."),
            "토글 함수를 넣습니다."
        );
        assert_eq!(
            without_leading_english_filler("Let me 확인하겠습니다."),
            "확인하겠습니다."
        );
        // Mid-stream, before the Hangul that follows has arrived.
        assert_eq!(without_leading_english_filler("Now"), "");
        assert_eq!(without_leading_english_filler("Now "), "");
    }

    /// Cutting these would change what the answer says, so they stay whole.
    #[test]
    fn an_answer_that_needs_its_english_opener_keeps_it() {
        // A quoted mention of the banned word is the subject, not a label.
        assert_eq!(
            without_leading_english_filler("`Now`을 금지했습니다."),
            "`Now`을 금지했습니다."
        );
        // An all-English sentence is a different violation; a cut leaves nonsense.
        assert_eq!(
            without_leading_english_filler("Now the tile view logic."),
            "Now the tile view logic."
        );
        // A word that merely starts with a filler is not a filler.
        assert_eq!(
            without_leading_english_filler("Firstly 확인합니다."),
            "Firstly 확인합니다."
        );
        assert_eq!(
            without_leading_english_filler("파일을 고쳤습니다."),
            "파일을 고쳤습니다."
        );
    }

    #[test]
    fn a_rendered_answer_drops_its_english_opener() {
        let lines = block_lines(
            &Block::new(
                BlockKind::Assistant,
                "Claude",
                "Now 상태 필드를 추가합니다.",
            ),
            80,
        );
        let text = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<String>();

        assert!(text.contains("상태 필드를 추가합니다."));
        assert!(!text.contains("Now"));
    }

    #[test]
    fn fullscreen_selection_preserves_response_and_thinking_icons() {
        // The bubble's own edge rows come first, so the marker row is found by
        // its prefix rather than by position.
        let assistant = block_lines(&Block::new(BlockKind::Assistant, "Codex", "answer"), 80)
            .into_iter()
            .find(|line| line.prefix == "  ")
            .expect("response marker row");
        let thinking = block_lines(
            &Block::new(BlockKind::Reasoning, THINKING_TITLE, "thought"),
            80,
        )
        .remove(0);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = vec![assistant.clone(), thinking.clone()];

        let full_row = |line: &PaintLine, row| CellRange {
            start: CellPosition { column: 0, row },
            end: CellPosition {
                column: painted_line_width(line).saturating_sub(1) as u16,
                row,
            },
        };
        assert_eq!(
            selection_columns_for_line(&assistant, full_row(&assistant, 0), 0),
            Some(2..8)
        );
        assert_eq!(
            selection_columns_for_line(&thinking, full_row(&thinking, 1), 1),
            Some(0..9)
        );

        assert!(renderer.begin_selection(0, 0));
        assert!(renderer.update_selection(7, 0));
        assert_eq!(
            renderer.finish_selection(7, 0),
            SelectionResult::Copy("answer".to_owned())
        );
        assert!(renderer.begin_selection(0, 1));
        assert!(renderer.update_selection(8, 1));
        assert_eq!(
            renderer.finish_selection(8, 1),
            SelectionResult::Copy("∴ thought".to_owned())
        );
    }

    #[test]
    fn fullscreen_selection_keeps_indented_bullet_markers() {
        let plan = block_lines(&Block::new(BlockKind::Plan, "Plan", "- first"), 80)
            .into_iter()
            .find(|line| line.prefix == "- ")
            .expect("plan bullet");
        let list = block_lines(&Block::new(BlockKind::System, "Notice", "- first"), 80)
            .into_iter()
            .find(|line| line.prefix == "  - ")
            .expect("list bullet");
        let full_row = |line: &PaintLine, row| CellRange {
            start: CellPosition { column: 0, row },
            end: CellPosition {
                column: painted_line_width(line).saturating_sub(1) as u16,
                row,
            },
        };

        assert_eq!(
            selection_columns_for_line(&plan, full_row(&plan, 0), 0),
            Some(0..painted_line_width(&plan))
        );
        assert_eq!(
            selection_columns_for_line(&list, full_row(&list, 0), 0),
            Some(2..painted_line_width(&list))
        );
        let deeper =
            wrapped_line("    - ", Tone::Accent, "second", Tone::Plain, false, 80).remove(0);
        assert_eq!(
            selection_columns_for_line(&deeper, full_row(&deeper, 0), 0),
            Some(4..painted_line_width(&deeper))
        );
    }

    #[test]
    fn fullscreen_selection_copies_unframed_code_and_indented_bullets() {
        let lines = block_lines(
            &Block::new(
                BlockKind::Assistant,
                "Codex",
                "```powershell\ndvz-debug\n```\n  - 실행 대상",
            ),
            80,
        );
        let code = lines
            .iter()
            .find(|line| line.text == "dvz-debug")
            .expect("unframed code row");
        // A response list keeps the reply marker instead of repeating "- " on
        // every row, so the continuation rows carry a plain indent.
        let bullet = lines
            .iter()
            .find(|line| line.text == "실행 대상")
            .expect("indented bullet");
        assert_eq!(bullet.prefix, "  ");
        let full_row = |line: &PaintLine, row| CellRange {
            start: CellPosition { column: 0, row },
            end: CellPosition {
                column: painted_line_width(line).saturating_sub(1) as u16,
                row,
            },
        };

        // The bubble pads every row out to a common width; that filler is not
        // part of the row's text.
        assert_eq!(
            selection_columns_for_line(code, full_row(code, 0), 0),
            Some(2..bubble_content_columns(code).end)
        );
        assert_eq!(
            selection_columns_for_line(bullet, full_row(bullet, 0), 0),
            Some(2..bubble_content_columns(bullet).end)
        );

        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = vec![bullet.clone()];
        assert!(renderer.begin_selection(0, 0));
        assert!(renderer.update_selection(painted_line_width(bullet).saturating_sub(1) as u16, 0));
        assert_eq!(
            renderer.finish_selection(painted_line_width(bullet).saturating_sub(1) as u16, 0),
            SelectionResult::Copy("실행 대상".to_owned())
        );
    }

    #[test]
    fn fullscreen_selection_excludes_blank_continuation_gutter_under_a_bullet() {
        let lines = wrapped_line(
            "  - ",
            Tone::Accent,
            "first second third",
            Tone::Plain,
            false,
            12,
        );
        let continuation = &lines[1];
        let range = CellRange {
            start: CellPosition { column: 0, row: 1 },
            end: CellPosition {
                column: painted_line_width(continuation).saturating_sub(1) as u16,
                row: 1,
            },
        };

        assert_eq!(
            selection_columns_for_line(continuation, range, 1),
            Some(4..painted_line_width(continuation))
        );
    }

    #[test]
    fn fullscreen_selection_excludes_the_blank_response_gutter_after_a_bullet() {
        let lines = block_lines(
            &Block::new(BlockKind::Assistant, "Codex", "first\nsecond"),
            80,
        );
        // Row 0 is the bubble's top edge, so the marker and its continuation are
        // the two rows under it.
        let second_row = CellRange {
            start: CellPosition { column: 0, row: 2 },
            end: CellPosition { column: 7, row: 2 },
        };
        assert_eq!(
            selection_columns_for_line(&lines[2], second_row, 2),
            Some(2..8)
        );
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = lines;

        assert!(renderer.begin_selection(0, 1));
        assert!(renderer.update_selection(7, 2));
        assert_eq!(
            renderer.finish_selection(7, 2),
            SelectionResult::Copy("first\nsecond".to_owned())
        );
    }

    #[test]
    fn fullscreen_selection_excludes_an_empty_response_row_gutter() {
        let lines = block_lines(
            &Block::new(BlockKind::Assistant, "Codex", "first\n\nsecond"),
            80,
        );
        let (row, blank) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.prefix == "  " && line.text.is_empty())
            .expect("blank response row");
        let range = CellRange {
            start: CellPosition { column: 0, row },
            end: CellPosition {
                column: painted_line_width(blank).saturating_sub(1) as u16,
                row,
            },
        };

        assert_eq!(selection_columns_for_line(blank, range, row), None);
    }

    #[test]
    fn fullscreen_selection_copies_right_aligned_user_bubbles() {
        let lines = block_lines(&Block::new(BlockKind::User, "You", "first\nsecond"), 80);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = lines;

        // The bubble sits at the right edge with a cell of padding inside it, so
        // its text starts one column in from the band.
        assert!(renderer.begin_selection(71, 1));
        assert!(renderer.update_selection(78, 2));
        assert_eq!(
            renderer.finish_selection(78, 2),
            SelectionResult::Copy("first\nsecond".to_owned())
        );
    }

    #[test]
    fn chat_bubbles_keep_left_padding_and_a_visible_right_gap() {
        CHAT_LAYOUT.store(true, Ordering::Relaxed);
        let user = block_lines(&Block::new(BlockKind::User, "You", "input"), 80);
        let assistant = block_lines(&Block::new(BlockKind::Assistant, "Codex", "output"), 80);
        let user_text = user
            .iter()
            .find(|line| line.tone == Tone::UserPrompt)
            .expect("user text row");
        let assistant_text = assistant
            .iter()
            .find(|line| line.text.contains("output"))
            .expect("assistant text row");
        let assistant_right_padding = assistant_text
            .tail
            .iter()
            .filter(|span| span.tone == Tone::AssistantBubble)
            .map(|span| UnicodeWidthStr::width(span.text.as_str()))
            .sum::<usize>();

        assert_eq!(user_text.text, "input  ");
        assert!(user_text.prefix.ends_with("› "));
        assert!(user_text.text.ends_with("  "));
        assert_eq!(assistant_right_padding, 2);

        let mut frame = CellFrame::new(80, 2);
        paint_line_into_frame(&mut frame, 0, user_text, None, None, None);
        paint_line_into_frame(&mut frame, 1, assistant_text, None, None, None);
        let user_right = painted_line_width(user_text) - 1;
        let assistant_right = painted_line_width(assistant_text) - 1;
        assert_eq!(frame.cell(user_right, 0).glyph, " ");
        assert_eq!(
            frame.cell(user_right, 0).style.background,
            word_background(Tone::UserPrompt)
        );
        assert_eq!(frame.cell(assistant_right, 1).glyph, " ");
        assert_eq!(
            frame.cell(assistant_right, 1).style.background,
            Some(assistant_bubble_background())
        );
    }

    #[test]
    fn user_prompt_bubble_cannot_start_selection_outside_its_text() {
        let lines = block_lines(&Block::new(BlockKind::User, "You", "prompt"), 80);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = lines;

        assert!(!renderer.begin_selection(0, 0));
        assert!(!renderer.begin_selection(72, 0));
        assert!(renderer.begin_selection(72, 1));
    }

    #[test]
    fn plan_update_repaints_all_rows_when_panel_geometry_changes() {
        let previous = CellFrame::new(8, 2);
        let mut current = previous.clone();
        current.write(0, 0, "새 작업", CellStyle::plain());

        let mut output = Vec::new();
        emit_synchronized_frame_diff_with_full_rows(
            &mut output,
            Some(&previous),
            &current,
            &[0],
            true,
            Some((0, 1, true)),
            false,
        )
        .expect("plan update emits");

        let output = String::from_utf8(output).expect("terminal bytes are UTF-8");
        assert!(output.starts_with("\x1b[?2026h"));
        assert!(output.contains("새 작업"));
        assert!(output.contains("\x1b[2;1H"));
        assert!(output.ends_with("\x1b[?2026l"));
        assert!(
            output.rfind("\x1b[?25h").unwrap() < output.rfind("\x1b[?2026l").unwrap(),
            "a hidden cursor is shown again inside the synchronized frame"
        );
    }

    #[test]
    fn plan_state_update_repaints_only_its_changed_row() {
        let mut previous = CellFrame::new(16, 3);
        previous.write(0, 0, "이전 작업", CellStyle::plain());
        previous.write(0, 2, "composer", CellStyle::plain());
        let mut current = previous.clone();
        current.write(0, 0, "새 작업", CellStyle::plain());

        let mut output = Vec::new();
        emit_synchronized_frame_diff_with_full_rows(
            &mut output,
            Some(&previous),
            &current,
            &[0],
            false,
            Some((0, 2, true)),
            true,
        )
        .expect("plan state update emits");

        let output = String::from_utf8(output).expect("terminal bytes are UTF-8");
        assert!(output.contains("새 작업"));
        assert_eq!(output.matches("새 작업").count(), 1);
        assert!(!output.contains("composer"));
        assert_eq!(output.matches("\x1b[?25l").count(), 1);
        assert_eq!(output.matches("\x1b[?25h").count(), 1);
    }

    #[test]
    fn a_visible_cursor_is_not_toggled_between_frames() {
        let previous = CellFrame::new(8, 2);
        let mut current = previous.clone();
        current.write(1, 1, "x", CellStyle::plain());

        let mut output = Vec::new();
        emit_synchronized_frame_diff_with_full_rows(
            &mut output,
            Some(&previous),
            &current,
            &[],
            false,
            Some((0, 1, true)),
            true,
        )
        .expect("frame emits");

        let output = String::from_utf8(output).expect("terminal bytes are UTF-8");
        // 표시 상태가 그대로면 커서 escape 없이 위치만 되돌린다. Hide/Show를 반복하면
        // 깜빡임 위상이 초기화돼 커서가 떨린다.
        assert!(!output.contains("\x1b[?25l"));
        assert!(!output.contains("\x1b[?25h"));
        assert!(output.contains("\x1b[2;1H"));
    }

    #[test]
    fn a_remote_row_update_protects_and_restores_the_visible_cursor() {
        let previous = CellFrame::new(8, 2);
        let mut current = previous.clone();
        current.write(0, 0, "x", CellStyle::plain());

        let mut output = Vec::new();
        emit_synchronized_frame_diff_with_full_rows(
            &mut output,
            Some(&previous),
            &current,
            &[],
            false,
            Some((0, 1, true)),
            true,
        )
        .expect("remote frame emits");

        let output = String::from_utf8(output).expect("terminal bytes are UTF-8");
        assert_eq!(output.matches("\x1b[?25l").count(), 1);
        assert_eq!(output.matches("\x1b[?25h").count(), 1);
        assert!(output.rfind("\x1b[?25h").unwrap() < output.rfind("\x1b[?2026l").unwrap());
    }

    #[test]
    fn a_remote_row_change_is_detected_outside_the_cursor_row() {
        let previous = CellFrame::new(8, 2);
        let mut current = previous.clone();
        current.write(0, 0, "x", CellStyle::plain());

        assert!(frame_changed_outside_row(Some(&previous), &current, 1));
        assert!(!frame_changed_outside_row(Some(&previous), &current, 0));
    }

    #[test]
    fn multiline_user_prompt_selection_excludes_the_left_gutter() {
        CHAT_LAYOUT.store(false, Ordering::Relaxed);
        let lines = user_prompt_lines(&Block::new(BlockKind::User, "You", "first\nsecond"), 80);
        let range = CellRange {
            start: CellPosition { column: 2, row: 1 },
            end: CellPosition {
                column: painted_line_width(&lines[2]).saturating_sub(1) as u16,
                row: 2,
            },
        };

        assert_eq!(selection_columns_for_line(&lines[2], range, 2), Some(2..8));
    }

    #[test]
    fn user_prompt_group_keeps_each_bubble_inside_the_right_column() {
        CHAT_LAYOUT.store(true, Ordering::Relaxed);
        let lines = block_group_lines(
            &Block::new(BlockKind::User, "You", "first\nsecond"),
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Expand,
            false,
        );

        assert_eq!(lines.len(), 5);
        // Every row is filled out to the longest one so the bubble paints square.
        assert_eq!(lines[0].tone, Tone::UserPromptPadding);
        assert_eq!(lines[1].text, "first   ");
        assert_eq!(lines[2].text, "second  ");
        assert!(lines[1].prefix.ends_with("› "));
        assert_eq!(lines[3].tone, Tone::UserPromptPadding);
        assert!(lines[4] == PaintLine::blank());

        let selection = CellRange {
            start: CellPosition { column: 71, row: 1 },
            end: CellPosition { column: 77, row: 2 },
        };
        assert_eq!(selection_columns_for_line(&lines[0], selection, 0), None);
        assert_ne!(selection_columns_for_line(&lines[1], selection, 1), None);
    }

    #[test]
    fn multiline_user_prompt_uses_the_longest_line_as_one_right_bubble() {
        CHAT_LAYOUT.store(true, Ordering::Relaxed);
        let lines = user_prompt_lines(
            &Block::new(BlockKind::User, "You", "longest line\nshort"),
            80,
        );

        assert_eq!(lines[0].tone, Tone::UserPromptPadding);
        assert_eq!(
            UnicodeWidthStr::width(lines[1].prefix.as_str()),
            UnicodeWidthStr::width(lines[2].prefix.as_str())
        );
        assert_eq!(painted_line_width(&lines[1]), painted_line_width(&lines[2]));
        assert_eq!(lines[2].text.trim(), "short");
        assert!(lines[1].prefix.ends_with("› "));
        assert!(lines[2].prefix.ends_with("  "));
        assert_eq!(lines[3].tone, Tone::UserPromptPadding);
    }

    #[test]
    fn selection_keeps_the_status_line_leading_space() {
        let line = status_line_row(None, "status", 20);
        let range = CellRange {
            start: CellPosition { column: 0, row: 0 },
            end: CellPosition { column: 6, row: 0 },
        };

        assert_eq!(selection_columns_for_line(&line, range, 0), Some(0..7));
    }

    #[test]
    fn chat_messages_use_opposite_80_percent_anchors() {
        let user = block_lines(&Block::new(BlockKind::User, "You", "x".repeat(120)), 80);
        let assistant = block_lines(
            &Block::new(BlockKind::Assistant, "Codex", "x".repeat(120)),
            80,
        );

        assert_eq!(conversation_region_width(80), 63);
        assert_eq!(UnicodeWidthStr::width(user[1].prefix.as_str()), 20);
        assert_eq!(UnicodeWidthStr::width(user[1].text.as_str()), 59);
        assert!(
            user.iter()
                .filter(|line| line.tone == Tone::UserPrompt)
                .all(|line| UnicodeWidthStr::width(line.prefix.as_str()) >= 16
                    && painted_width(line) == 79)
        );
        assert!(
            assistant
                .iter()
                .filter(|line| !line.text.is_empty())
                .all(|line| painted_width(line) <= 64)
        );
    }

    #[test]
    fn status_line_effort_icon_survives_copy_for_composer_paste() {
        let line = status_line_row(
            Some(StatusLineView {
                model: None,
                effort: Some("high".to_owned()),
                context: None,
                five_hour_percent: None,
                five_hour_remaining: None,
                weekly_percent: None,
                notice: None,
            }),
            "",
            80,
        );
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = vec![line];

        assert!(renderer.begin_selection(0, 0));
        assert!(renderer.update_selection(6, 0));
        let SelectionResult::Copy(copied) = renderer.finish_selection(6, 0) else {
            panic!("status line selection should copy");
        };
        assert_eq!(copied, "◆ high");

        let mut editor = Editor::default();
        editor.insert_paste_str(&copied);
        assert_eq!(composer_display(&editor, &[]).0, "◆ high");
    }

    #[test]
    fn composer_display_keeps_image_paths_as_plain_text() {
        let mut editor = Editor::default();
        editor.set_text(r"look C:\tmp\first.PNG then /tmp/second.webp");

        let (display, cursor) = composer_display(&editor, &[]);

        assert_eq!(display, r"look C:\tmp\first.PNG then /tmp/second.webp");
        assert_eq!(cursor, display.chars().count());
    }

    #[test]
    fn composer_display_preserves_cursor_for_all_paths() {
        let mut editor = Editor::default();
        editor.set_text("open /tmp/report.txt and /tmp/photo.jpeg");
        editor.move_home();
        for _ in 0..31 {
            editor.move_right();
        }

        let (display, cursor) = composer_display(&editor, &[]);

        assert_eq!(display, "open /tmp/report.txt and /tmp/photo.jpeg");
        assert_eq!(cursor, 31);
    }

    #[test]
    fn composer_display_shows_labels_only_for_explicit_image_attachments() {
        let mut editor = Editor::default();
        editor.set_text(r"inspect C:\tmp\ordinary.png");
        editor.insert_attachment();
        let images = vec![r"C:\Temp\clipboard-image.bmp".to_owned()];

        let (display, cursor) = composer_display(&editor, &images);

        assert_eq!(display, r"inspect C:\tmp\ordinary.png [Image #1]");
        assert_eq!(cursor, display.chars().count());
    }

    #[test]
    fn input_lines_show_image_labels_when_the_text_editor_is_empty() {
        let mut editor = Editor::default();
        editor.insert_attachment();
        let images = vec![r"C:\Temp\clipboard-image.bmp".to_owned()];

        let (rows, cursor_row, cursor_col, _) =
            input_lines(&editor, &images, 80, "", "", None, None);

        assert!(painted(&rows[1]).contains("> [Image #1]"));
        assert_eq!(cursor_row, 1);
        assert_eq!(cursor_col, UnicodeWidthStr::width("│ > [Image #1]"));
    }

    #[test]
    fn image_attachment_cursor_moves_across_the_whole_label() {
        let mut editor = Editor::default();
        editor.insert_attachment();
        let images = vec![r"C:\Temp\clipboard-image.bmp".to_owned()];

        let (display, after) = composer_display(&editor, &images);
        editor.move_left();
        let (_, before) = composer_display(&editor, &images);
        editor.move_right();
        let (_, after_again) = composer_display(&editor, &images);

        assert_eq!(before, 0);
        assert_eq!(after, display.chars().count());
        assert_eq!(after_again, after);
    }

    #[test]
    fn composer_display_keeps_an_incomplete_absolute_path_at_the_cursor() {
        let mut editor = Editor::default();
        editor.set_text(r"C:\Users\me\AppData\Local\Temp\clipboard");

        let (display, cursor) = composer_display(&editor, &[]);

        assert_eq!(display, r"C:\Users\me\AppData\Local\Temp\clipboard");
        assert_eq!(cursor, display.chars().count());
    }

    #[test]
    fn composer_display_keeps_a_slash_command_as_text() {
        let mut editor = Editor::default();
        editor.set_text("/");

        assert_eq!(composer_display(&editor, &[]).0, "/");
    }

    #[test]
    fn composer_display_collapses_a_large_paste_without_losing_the_editor_text() {
        let mut editor = Editor::default();
        editor.insert_paste_str("one\ntwo\nthree\nfour\nfive\nsix");

        let (display, cursor) = composer_display(&editor, &[]);

        assert_eq!(display, "[Pasted text · 6 lines]");
        assert_eq!(cursor, display.chars().count());
        assert_eq!(editor.text(), "one\ntwo\nthree\nfour\nfive\nsix");
    }

    #[test]
    fn composer_display_keeps_a_large_paste_collapsed_after_a_newline_and_tail_text() {
        let mut editor = Editor::default();
        editor.insert_paste_str("one\ntwo\nthree\nfour\nfive\nsix");
        editor.newline();
        editor.insert_str("after");

        assert_eq!(
            composer_display(&editor, &[]).0,
            "[Pasted text · 6 lines]\nafter"
        );
    }

    #[test]
    fn composer_display_keeps_text_entered_before_a_large_paste_visible() {
        let mut editor = Editor::default();
        editor.set_text("before ");
        editor.insert_paste_str("one\ntwo\nthree\nfour\nfive\nsix");

        assert_eq!(
            composer_display(&editor, &[]).0,
            "before [Pasted text · 6 lines]"
        );
    }

    #[test]
    fn update_card_uses_a_heading_and_one_row_per_item() {
        let block = Block::new(
            BlockKind::Update,
            "Update Available",
            "첫 번째 안내\n두 번째 안내",
        );
        for width in [24u16, 80] {
            let lines = block_lines(&block, width);

            assert_eq!(lines.len(), 7);
            assert!(painted(&lines[0]).starts_with("┌── Update Available "));
            assert!(painted(&lines[0]).ends_with('┐'));
            assert_eq!(
                UnicodeWidthStr::width(painted(&lines[0]).as_str()),
                width as usize - 1
            );
            assert!(lines[1].text.is_empty());
            assert_eq!(lines[2].prefix, "  •  ");
            assert_eq!(lines[2].text, "첫 번째 안내");
            assert_eq!(lines[3].prefix, "  •  ");
            assert_eq!(lines[3].text, "두 번째 안내");
            assert!(lines[4].text.is_empty());
            assert!(painted(&lines[5]).starts_with('└'));
            assert!(painted(&lines[5]).ends_with('┘'));
            assert_eq!(
                UnicodeWidthStr::width(painted(&lines[5]).as_str()),
                width as usize - 1
            );
            assert!(lines[6].text.is_empty());
        }
    }

    #[test]
    fn startup_tip_fits_its_longest_item_with_ten_right_padding_columns() {
        let longest = "/side-panel (Alt + P): Toggle side panel";
        let block = Block::new(
            BlockKind::Update,
            "Tip",
            format!("/provider: Set provider\n{longest}\nShift + ↑↓ model · ←→ effort"),
        );
        let lines = block_lines(&block, 80);
        let expected_width = UnicodeWidthStr::width(longest) + 15;
        let longest_line = lines
            .iter()
            .find(|line| line.text == longest)
            .expect("longest tip");

        assert_eq!(
            UnicodeWidthStr::width(painted(&lines[0]).as_str()),
            expected_width
        );
        assert_eq!(
            UnicodeWidthStr::width(painted(&lines[lines.len() - 2]).as_str()),
            expected_width
        );
        assert_eq!(
            expected_width - UnicodeWidthStr::width(painted(longest_line).as_str()),
            10
        );

        let narrow = block_lines(&block, 24);
        assert_eq!(UnicodeWidthStr::width(painted(&narrow[0]).as_str()), 23);
        assert_eq!(
            UnicodeWidthStr::width(painted(&narrow[narrow.len() - 2]).as_str()),
            23
        );
        assert!(
            narrow[2..narrow.len() - 2]
                .iter()
                .filter(|line| !painted(line).is_empty())
                .all(|line| UnicodeWidthStr::width(painted(line).as_str()) <= 21)
        );
    }

    #[test]
    fn first_plan_removes_only_the_startup_tip() {
        for mode in [RenderMode::Fullscreen, RenderMode::Inline] {
            let startup = Block::new(BlockKind::Update, "Tip", "/provider");
            let available = Block::new(
                BlockKind::Update,
                "Update Available",
                "New version 1.3.13 is available.",
            );
            let answer = Block::new(BlockKind::Assistant, "Codex", "답변");
            let mut renderer = Renderer::new(ThemeKind::Minimal, mode);
            renderer.history = vec![startup, available, answer];
            renderer.wrapped_width = 80;

            assert!(renderer.remove_startup_update_from_history());
            assert_eq!(
                renderer
                    .history
                    .iter()
                    .map(|block| block.title.as_str())
                    .collect::<Vec<_>>(),
                ["Update Available", "Codex"]
            );
            assert_eq!(renderer.wrapped_width, 0);
            assert!(!renderer.remove_startup_update_from_history());
        }
    }

    #[test]
    fn model_change_uses_a_background_card_and_indented_turn_marker() {
        let block = Block::new(
            BlockKind::ModelChange,
            "Model changed",
            "↳ GPT-5.6 Terra · xhigh",
        );
        let lines = block_lines(&block, 80);

        assert_eq!(lines.len(), 3);
        assert!(lines[..2].iter().all(|line| line.tone == Tone::ModelChange));
        assert_eq!(lines[1].prefix, "    ");
        assert!(lines[1].text.starts_with('↳'));
        assert!(lines[2].text.is_empty());
    }

    #[test]
    fn assistant_visual_wraps_are_marked_for_copy_reconstruction() {
        let lines = wrapped_line(
            "● ",
            Tone::Accent,
            "this response wraps across multiple terminal rows",
            Tone::Plain,
            false,
            18,
        );

        assert!(lines.len() > 1);
        assert!(lines[..lines.len() - 1].iter().all(copy_joins_next));
        assert!(!copy_joins_next(lines.last().expect("last row")));
    }

    #[test]
    fn wrapped_lines_reserve_the_final_column_from_autowrap() {
        let lines = wrapped_line("● ", Tone::Accent, "abcdef", Tone::Plain, false, 8);

        assert_eq!(
            lines.iter().map(painted_width).collect::<Vec<_>>(),
            vec![7, 3]
        );
        assert_eq!(painted(&lines[0]), "● abcde");
        assert_eq!(painted(&lines[1]), "  f");
    }

    #[test]
    fn prompt_border_is_never_painted_as_selected() {
        CHAT_LAYOUT.store(false, Ordering::Relaxed);
        let width = 80u16;
        let body = "eGhis.Chart.Support\teGhis.Chart.Support.Views.검안실\neGhis.Chart.Support\teGhis.Chart.Support.Views.검안실\n\n이렇게보낸건데";
        let lines = user_prompt_lines(&Block::new(BlockKind::User, "You", body), width);
        assert!(lines.iter().any(|line| line.prefix.starts_with('▌')));

        let range = CellRange {
            start: CellPosition { column: 2, row: 1 },
            end: CellPosition {
                column: width - 2,
                row: lines.len() - 1,
            },
        };
        let mut frame = CellFrame::new(usize::from(width), lines.len());
        for (row, line) in lines.iter().enumerate() {
            let selected = selection_columns_for_line(line, range, row);
            paint_line_into_frame(&mut frame, row, line, selected, None, None);
        }

        for row in 0..lines.len() {
            assert_ne!(
                frame.cell(0, row).style.background,
                Some(theme::selection_bg()),
                "row {row} border was painted as selected"
            );
        }
    }

    #[test]
    fn tab_separated_prompt_rows_lose_no_characters_when_painted() {
        CHAT_LAYOUT.store(false, Ordering::Relaxed);
        let body = "eGhis.Chart.Support\teGhis.Chart.Support.Views.검안실.H3EyeClinicImageView\tlab_cv\tuse_cv4\t1\t100\t4.010\t영상관연계여부\tart.SuComboBox\t사용,미사용\t미사용\t지스랩에서 영상관리4.0을 열 수 있도록\tUSER";
        let block = Block::new(BlockKind::User, "You", body);
        let width = 100u16;
        let lines = user_prompt_lines(&block, width);

        let mut frame = CellFrame::new(usize::from(width), lines.len());
        for (row, line) in lines.iter().enumerate() {
            paint_line_into_frame(&mut frame, row, line, None, None, None);
        }

        let painted = (0..lines.len())
            .flat_map(|row| (0..usize::from(width)).map(move |column| (column, row)))
            .map(|(column, row)| frame.cell(column, row).glyph.clone())
            .collect::<String>();
        let strip = |text: &str| {
            text.chars()
                .filter(|ch| !ch.is_whitespace() && *ch != '▌')
                .collect::<String>()
        };

        assert_eq!(strip(&painted), strip(body));
    }

    #[test]
    fn wrapped_lines_expand_tabs_to_the_next_tab_stop() {
        let lines = wrapped_line("▌ ", Tone::Accent, "ab\tcd", Tone::Plain, false, 40);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "ab      cd");
    }

    #[test]
    fn painted_cells_never_carry_a_control_character() {
        let mut frame = CellFrame::new(12, 1);
        frame.write(0, 0, "a\tb", CellStyle::plain());

        let glyphs = (0..3)
            .map(|column| frame.cell(column, 0).glyph.clone())
            .collect::<Vec<_>>();
        // 탭이 차지하던 칸은 빈 칸으로 남아 배경이 그대로 칠해지고, 뒤따르는
        // 글자는 폭 계산과 같은 열에 그려진다.
        assert_eq!(glyphs, vec!["a".to_owned(), String::new(), "b".to_owned()]);
    }

    #[test]
    fn wrapped_lines_keep_korean_words_together() {
        let lines = wrapped_line("● ", Tone::Accent, "가나다 라마바", Tone::Plain, false, 10);

        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["가나다", "라마바"]
        );
    }

    #[test]
    fn markdown_lines_move_links_to_the_next_row_as_a_word() {
        let lines = markdown_line(
            "● ",
            Tone::Accent,
            "가나다 [state.rs](src/state.rs:4694)",
            Tone::Plain,
            false,
            22,
        );

        assert_eq!(painted(&lines[0]), "● 가나다");
        assert_eq!(painted(&lines[1]), "  state.rs:4694");
    }

    #[test]
    fn code_highlighter_distinguishes_keywords_types_functions_and_literals() {
        let spans = highlight_code(
            "pub struct DevezClient { retries: 3, name: \"cli\", run: build() } // ready",
            "rust",
        );

        assert!(
            spans
                .iter()
                .any(|span| { span.text.contains("pub") && span.tone == Tone::SyntaxKeyword })
        );
        assert!(spans.iter().any(|span| {
            span.text == "DevezClient" && span.tone == Tone::SyntaxType && span.bold
        }));
        assert!(
            spans
                .iter()
                .any(|span| { span.text.contains('3') && span.tone == Tone::SyntaxNumber })
        );
        assert!(
            spans
                .iter()
                .any(|span| { span.text.contains("\"cli\"") && span.tone == Tone::SyntaxString })
        );
        assert!(
            spans
                .iter()
                .any(|span| { span.text == "build" && span.tone == Tone::SyntaxFunction })
        );
        assert!(
            spans
                .iter()
                .any(|span| { span.text.contains("// ready") && span.tone == Tone::SyntaxComment })
        );
    }

    #[test]
    fn code_highlighter_handles_sql_and_block_comments() {
        let sql = highlight_code("SELECT id FROM users -- active rows", "sql");
        assert!(sql.iter().any(|span| {
            span.text == "SELECT" && span.tone == Tone::SyntaxKeyword && span.bold
        }));
        assert!(sql.iter().any(|span| {
            span.text.contains("-- active rows") && span.tone == Tone::SyntaxComment
        }));

        let rust = highlight_code("let result: DevezClient = true; /* ready */", "rust");
        assert!(rust.iter().any(|span| {
            span.text == "DevezClient" && span.tone == Tone::SyntaxType && span.bold
        }));
        assert!(
            rust.iter()
                .any(|span| span.text == "true" && span.tone == Tone::SyntaxNumber)
        );
        assert!(
            rust.iter().any(|span| {
                span.text.contains("/* ready */") && span.tone == Tone::SyntaxComment
            })
        );
    }

    #[test]
    fn hash_comments_require_a_comment_boundary() {
        let shell = highlight_code("echo value#suffix # comment", "bash");
        assert!(shell.iter().any(|span| {
            span.text.contains("value#suffix") && span.tone != Tone::SyntaxComment
        }));
        assert!(
            shell
                .iter()
                .any(|span| { span.text == "# comment" && span.tone == Tone::SyntaxComment })
        );
    }

    #[test]
    fn diff_renderer_assigns_add_remove_and_header_semantics() {
        assert_eq!(
            diff_line("", Tone::Plain, "+added()", 80)[0].tone,
            Tone::DiffAdded
        );
        assert_eq!(
            diff_line("", Tone::Plain, "-removed()", 80)[0].tone,
            Tone::DiffRemoved
        );
        assert_eq!(
            diff_line("", Tone::Plain, "@@ -1 +1 @@", 80)[0].tone,
            Tone::DiffHeader
        );
        assert_eq!(
            diff_line("", Tone::Plain, "+++ b/src/main.rs", 80)[0].tone,
            Tone::DiffHeader
        );
    }

    #[test]
    fn markdown_table_renderer_draws_a_bordered_table() {
        let rows = vec![
            vec!["항목".to_owned(), "상태".to_owned()],
            vec!["릴리즈 빌드".to_owned(), "성공".to_owned()],
        ];
        let alignments = vec![TableAlignment::Left, TableAlignment::Center];

        let lines = markdown_table_lines("  ", Tone::Muted, &rows, &alignments, Tone::Plain, 80)
            .expect("table fits the available width");

        assert_eq!(lines[0].prefix, "  ");
        assert!(lines[0].text.starts_with("항목"));
        assert_eq!(lines[0].tone, Tone::MarkdownHeading);
        assert!(!lines[0].bold);
        assert!(
            lines[0]
                .tail
                .iter()
                .all(|span| span.tone != Tone::AssistantBubble)
        );
        assert!(lines[1].text.chars().all(|ch| ch == '─'));
        assert!(lines.last().expect("data row").text.contains("릴리즈 빌드"));
    }

    #[test]
    fn assistant_list_keeps_the_response_marker_without_hyphens() {
        CHAT_LAYOUT.store(false, Ordering::Relaxed);
        let lines = block_lines_with_expansion(
            &Block::new(BlockKind::Assistant, "Codex", "- first\n- second"),
            80,
            false,
        );

        let second_item = lines
            .iter()
            .find(|line| line.text.trim() == "second")
            .expect("second list item");
        let first_item = lines
            .iter()
            .find(|line| line.text.trim() == "first")
            .expect("first list item");
        assert_eq!(first_item.prefix, RESPONSE_BULLET_PREFIX);
        assert_eq!(second_item.prefix, "  ");
        CHAT_LAYOUT.store(true, Ordering::Relaxed);
    }

    #[test]
    fn markdown_table_wraps_long_cells_and_honors_alignment() {
        let rows = vec![
            vec!["설명".to_owned(), "값".to_owned()],
            vec![
                "긴 설명도 열 안에서 여러 줄로 자연스럽게 표시됩니다".to_owned(),
                "42".to_owned(),
            ],
        ];
        let alignments = vec![TableAlignment::Left, TableAlignment::Right];

        let lines = markdown_table_lines("", Tone::Plain, &rows, &alignments, Tone::Plain, 30)
            .expect("table fits the available width");

        assert!(lines.len() > 3);
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(painted(line).as_str()) <= 29)
        );
        assert!(lines.iter().any(|line| painted(line).ends_with("42")));
    }

    #[test]
    fn markdown_table_in_a_response_is_rendered_as_a_table() {
        let block = Block::new(
            BlockKind::Warning,
            "상태",
            "| 항목 | 상태 |\n|---|---|\n| 릴리즈 빌드 | 성공 |",
        );

        let lines = block_lines(&block, 80);

        assert!(lines.iter().any(|line| line.tone == Tone::MarkdownHeading));
        assert!(!lines.iter().any(|line| line.text.contains("| 항목 |")));
        assert!(lines.iter().any(|line| line.text.contains("릴리즈 빌드")));
    }

    #[test]
    fn markdown_renderer_hides_markup_and_colors_inline_code() {
        let lines = markdown_line(
            "",
            Tone::Plain,
            "Use **DevezClient** with `ThemeKind`.",
            Tone::Plain,
            false,
            100,
        );
        let line = &lines[0];
        let rendered = std::iter::once(line.text.as_str())
            .chain(line.tail.iter().map(|span| span.text.as_str()))
            .collect::<String>();

        assert_eq!(rendered, "Use DevezClient with ThemeKind.");
        assert!(
            line.tail
                .iter()
                .any(|span| span.text == "ThemeKind" && span.tone == Tone::InlineCode)
        );
        assert!(
            line.tail
                .iter()
                .any(|span| span.text == "DevezClient" && span.bold)
        );
    }

    #[test]
    fn markdown_uses_distinct_tones_for_heading_link_and_inline_code() {
        let lines = markdown_line(
            "",
            Tone::Plain,
            "제목 [문서](https://example.com)와 `Config`",
            Tone::MarkdownHeading,
            true,
            100,
        );
        let line = &lines[0];

        assert_eq!(line.tone, Tone::MarkdownHeading);
        assert!(
            line.tail
                .iter()
                .any(|span| span.text == "문서" && span.tone == Tone::MarkdownLink)
        );
        assert!(
            line.tail
                .iter()
                .any(|span| span.text == "Config" && span.tone == Tone::InlineCode)
        );
    }

    #[test]
    fn powershell_code_block_renders_without_a_frame() {
        let lines = block_lines(
            &Block::new(
                BlockKind::Assistant,
                "Codex",
                "```powershell\ncargo run --release\n```",
            ),
            30,
        );
        let rendered = lines.iter().map(painted).collect::<Vec<_>>();

        // The bubble's edge row comes first and pads every row out to its width,
        // so the code row is matched by its text rather than by position.
        assert!(
            rendered
                .iter()
                .any(|line| line.trim_end() == "  cargo run --release")
        );
        assert!(
            rendered
                .iter()
                .all(|line| !line.contains(['┌', '┐', '└', '┘', '│']))
        );
    }

    /// The gutter is what makes a patch readable: `@@` headers are spent on line
    /// numbers, and each row lands under the number it has in the file.
    #[test]
    fn file_change_lines_number_the_patch_from_its_hunk_headers() {
        let lines = block_lines(
            &Block::new(
                BlockKind::FileChange,
                r"Update(src\main.rs)",
                "Added 2 lines, removed 1 line\n@@ -83,3 +90,4 @@\n context\n-let old = 1;\n+let new = 2;\n+let extra = 3;",
            ),
            80,
        );
        let rendered = lines
            .iter()
            .map(|line| {
                std::iter::once(line.prefix.as_str())
                    .chain(std::iter::once(line.text.as_str()))
                    .chain(line.tail.iter().map(|span| span.text.as_str()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            [
                r"● Update(src\main.rs)",
                "  ⎿ Added 2 lines, removed 1 line",
                "      90   context",
                "      84 - let old = 1;",
                "      91 + let new = 2;",
                "      92 + let extra = 3;",
            ]
        );
        // The counts stand out of the dim summary row.
        assert!(
            lines[1]
                .tail
                .iter()
                .any(|span| span.text == "2" && span.tone == Tone::Plain && span.bold)
        );
        assert_eq!(lines[1].text, "Added ");
        assert_eq!(lines[1].tone, Tone::Muted);
        // `print_line` paints the row tint from the tone, so these carry the
        // full-width red and green bands.
        assert_eq!(lines[3].tone, Tone::DiffRemoved);
        assert_eq!(lines[3].prefix_tone, Tone::DiffRemoved);
        assert_eq!(lines[4].tone, Tone::DiffAdded);
        assert_eq!(lines[2].tone, Tone::Plain);
        assert_eq!(lines[2].prefix_tone, Tone::Muted);
    }

    /// A rewritten line says more than "this row changed": the words that
    /// actually moved carry a stronger tint, and the words the two rows share are
    /// left on the row's own band.
    #[test]
    fn file_change_lines_tint_the_words_a_row_rewrote() {
        let lines = block_lines(
            &Block::new(
                BlockKind::FileChange,
                r"Update(src\main.rs)",
                "Added 1 line, removed 1 line\n@@ -1,1 +1,1 @@\n-let old = 1;\n+let new = 2;",
            ),
            80,
        );
        let spans = |line: &PaintLine| {
            std::iter::once((line.text.clone(), line.tone))
                .chain(line.tail.iter().map(|span| (span.text.clone(), span.tone)))
                .filter(|(text, _)| !text.is_empty())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            spans(&lines[2]),
            [
                ("let ".to_owned(), Tone::DiffRemoved),
                ("old".to_owned(), Tone::DiffRemovedWord),
                (" = ".to_owned(), Tone::DiffRemoved),
                ("1".to_owned(), Tone::DiffRemovedWord),
                (";".to_owned(), Tone::DiffRemoved),
            ]
        );
        assert_eq!(
            spans(&lines[3]),
            [
                ("let ".to_owned(), Tone::DiffAdded),
                ("new".to_owned(), Tone::DiffAddedWord),
                (" = ".to_owned(), Tone::DiffAdded),
                ("2".to_owned(), Tone::DiffAddedWord),
                (";".to_owned(), Tone::DiffAdded),
            ]
        );
        // The row still paints its full-width band, and only the changed words
        // deepen it.
        assert_eq!(
            row_background(Tone::DiffRemovedWord),
            row_background(Tone::DiffRemoved)
        );
        assert!(word_background(Tone::DiffRemovedWord).is_some());
        assert!(word_background(Tone::DiffRemoved).is_none());
    }

    /// Two lines that merely sit next to each other in a hunk are not a rewrite of
    /// each other, and tinting words they happen to share would invent an edit.
    #[test]
    fn word_diff_leaves_unrelated_rows_alone() {
        assert!(word_diff("let total = sum(values);", "return None;").is_none());
        // Nothing in common to leave untinted is the row band's job, not a word's.
        assert!(word_diff("alpha", "beta").is_none());
    }

    /// A sweeping refactor must not push the rest of the turn off screen.
    #[test]
    fn file_change_lines_count_rows_past_the_cap() {
        let body = ["Added 80 lines, removed 0 lines", "@@ -1,0 +1,80 @@"]
            .into_iter()
            .map(str::to_owned)
            .chain((1..=80).map(|n| format!("+row {n}")))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = block_lines(&Block::new(BlockKind::FileChange, "Update(a.rs)", body), 80);

        // Heading, summary, the capped rows, then the count.
        assert_eq!(lines.len(), FILE_CHANGE_ROWS + 3);
        assert_eq!(
            lines.last().expect("a count row").text,
            format!("… +{} lines", 80 - FILE_CHANGE_ROWS)
        );
    }

    #[test]
    fn diff_display_modes_hide_summarize_or_expand_a_file_change() {
        let block = Block::new(
            BlockKind::FileChange,
            "Update(a.rs)",
            "Added 2 lines, removed 1 line\n@@ -1 +1 @@\n-old\n+new\n+extra",
        );

        assert!(
            block_lines_with_mode(
                &block,
                80,
                ShellDisplayMode::Collapse,
                DiffDisplayMode::Hide,
                false,
            )
            .is_empty()
        );

        let collapsed = block_lines_with_mode(
            &block,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Collapse,
            false,
        );
        assert_eq!(collapsed.len(), 2);
        assert_eq!(painted(&collapsed[1]), "  ⎿ Added 2 · Removed 1");

        let expanded = block_lines_with_mode(
            &block,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Expand,
            false,
        );
        assert!(expanded.iter().any(|line| painted(line).contains("old")));
        assert!(expanded.iter().any(|line| painted(line).contains("new")));
    }

    #[test]
    fn collapsed_file_change_group_shows_one_card_with_combined_counts() {
        let group = Block::file_change_group(
            "Update(src/state.rs)",
            vec![
                Block::new(
                    BlockKind::FileChange,
                    "Update(src/state.rs)",
                    "Added 2 lines, removed 1 line\n@@ -1 +1,2 @@\n-old\n+new\n+extra",
                ),
                Block::new(
                    BlockKind::FileChange,
                    "Update(src/state.rs)",
                    "Added 1 line, removed 1 line\n@@ -3 +3 @@\n-before\n+after",
                ),
            ],
        );

        let collapsed = block_lines_with_mode(
            &group,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Collapse,
            false,
        );
        assert_eq!(collapsed.len(), 2);
        assert_eq!(painted(&collapsed[0]), "● Update(src/state.rs)");
        assert_eq!(painted(&collapsed[1]), "  ⎿ Added 3 · Removed 2");

        let expanded = block_lines_with_mode(
            &group,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Expand,
            false,
        );
        assert!(expanded.iter().any(|line| painted(line).contains("old")));
        assert!(expanded.iter().any(|line| painted(line).contains("after")));
        assert!(
            expanded
                .iter()
                .filter(|line| line.tool_heading.is_some())
                .all(|line| line.tool_heading == Some(group.id()))
        );
    }

    #[test]
    fn clicking_a_file_change_heading_toggles_its_patch_and_hover() {
        let block = Block::new(
            BlockKind::FileChange,
            "Update(a.rs)",
            "Added 1 line, removed 1 line\n@@ -1 +1 @@\n-old\n+new",
        );
        let id = block.id();
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.diff_display_mode = DiffDisplayMode::Collapse;
        renderer.history.push(block);
        renderer.rewrap(80);
        renderer.previous_lines = renderer.wrapped.clone();

        assert!(renderer.hover_at(3, 0));
        assert_eq!(renderer.hovered_tool, Some(id));
        assert!(renderer.toggle_tool_at(0));
        assert!(renderer.expanded_tools.contains(&id));
        assert!(
            renderer
                .wrapped
                .iter()
                .any(|line| painted(line).contains("old"))
        );

        assert!(renderer.toggle_tool_at(0));
        assert!(!renderer.expanded_tools.contains(&id));
        assert!(
            renderer
                .wrapped
                .iter()
                .all(|line| !painted(line).contains("old"))
        );

        renderer.diff_display_mode = DiffDisplayMode::Expand;
        renderer.rewrap(80);
        renderer.previous_lines = renderer.wrapped.clone();
        assert!(renderer.toggle_tool_at(0));
        assert!(renderer.expanded_tools.contains(&id));
        assert!(
            renderer
                .wrapped
                .iter()
                .all(|line| !painted(line).contains("old")),
            "the per-file toggle collapses an otherwise expanded diff"
        );
    }

    /// Summaries end with `[file](path:line)` links; the raw markdown used to wrap
    /// mid-path across two rows, so the label — plus the line number — is all we keep.
    #[test]
    fn markdown_renderer_collapses_links_to_their_label() {
        let lines = markdown_line(
            "",
            Tone::Plain,
            "변경: [src/main.rs](C:/Source/DevezVibe/src/main.rs:83), [Cargo.toml](C:/Source/DevezVibe/Cargo.toml:29)",
            Tone::Plain,
            false,
            200,
        );
        let line = &lines[0];
        let rendered = std::iter::once(line.text.as_str())
            .chain(line.tail.iter().map(|span| span.text.as_str()))
            .collect::<String>();

        assert_eq!(lines.len(), 1);
        assert_eq!(rendered, "변경: src/main.rs:83, Cargo.toml:29");
    }

    #[test]
    fn markdown_link_label_keeps_its_click_target() {
        let lines = markdown_line(
            "  ",
            Tone::Plain,
            "열기: [미리보기](file:///C:/Temp/preview.html)",
            Tone::Plain,
            false,
            80,
        );
        let line = &lines[0];
        let start = UnicodeWidthStr::width("  열기: ");

        assert_eq!(
            line.pick.as_ref().and_then(|picks| picks.at(start)),
            Some(Pick::OpenLink("file:///C:/Temp/preview.html".to_owned()))
        );
        assert_eq!(
            line.pick.as_ref().and_then(|picks| picks.at(start - 1)),
            None
        );
    }

    #[test]
    fn markdown_local_file_links_use_platform_openable_targets() {
        let lines = markdown_line(
            "",
            Tone::Plain,
            "[전체 프로세스 명세서](</D:/eGhisSource/eGhisCCC/docs/work-item-analysis-process-spec.md:83:12>)",
            Tone::Plain,
            false,
            100,
        );
        let line = &lines[0];

        assert_eq!(line.text, "전체 프로세스 명세서:83:12");
        assert_eq!(
            line.pick.as_ref().and_then(|picks| picks.at(0)),
            Some(Pick::OpenLink(
                "D:/eGhisSource/eGhisCCC/docs/work-item-analysis-process-spec.md".to_owned()
            ))
        );
    }

    #[test]
    fn markdown_link_target_normalization_preserves_non_file_urls() {
        assert_eq!(
            markdown_link_open_target("/D:/Docs/spec.md"),
            "D:/Docs/spec.md"
        );
        assert_eq!(
            markdown_link_open_target("D:\\Docs\\spec.md:83"),
            "D:\\Docs\\spec.md"
        );
        assert_eq!(
            markdown_link_open_target("file:///D:/Docs/spec.md:83:12"),
            "file:///D:/Docs/spec.md"
        );
        assert_eq!(
            markdown_link_open_target("https://example.com/releases/83"),
            "https://example.com/releases/83"
        );
        assert_eq!(
            markdown_link_open_target("HTTPS://example.com/file:83"),
            "HTTPS://example.com/file:83"
        );
    }

    #[test]
    fn markdown_links_are_underlined_in_both_render_modes() {
        assert!(cell_style(Tone::MarkdownLink, false, None, false).underlined);

        let mut output = Vec::new();
        set_tone(&mut output, Tone::MarkdownLink).expect("link tone renders");
        assert!(
            String::from_utf8(output)
                .expect("terminal bytes are UTF-8")
                .contains("\x1b[4m")
        );
    }

    #[test]
    fn markdown_renderer_leaves_bracket_text_alone() {
        let lines = markdown_line(
            "",
            Tone::Plain,
            "[Enter] 완료 후 계속",
            Tone::Plain,
            false,
            80,
        );
        assert_eq!(lines[0].text, "[Enter] 완료 후 계속");
        assert_eq!(line_suffix("https://example.com:8080"), None);
        assert_eq!(line_suffix("src/main.rs:83:12").as_deref(), Some(":83:12"));
    }

    /// The transient notice belongs to the composer bottom rule, leaving the spacer
    /// row above the input untouched.
    #[test]
    fn transient_notice_sits_on_the_composer_bottom_rule() {
        let editor = Editor::default();
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &[],
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: Some("• Copied to clipboard".to_owned()),
                composer_mode: None,
            },
            80,
        );

        let notice = frame
            .lines
            .iter()
            .position(|line| {
                line.tail
                    .iter()
                    .any(|span| span.text == "• Copied to clipboard")
            })
            .expect("notice row");
        assert_eq!(
            frame.lines[notice].tone,
            Tone::Border,
            "the composer rule should keep its border tone"
        );
        assert!(
            painted(&frame.lines[notice]).starts_with('╰'),
            "the notice should sit on the rule, not replace it"
        );
        assert_eq!(
            frame.lines[notice].tail[1].tone,
            Tone::Accent,
            "notice is off-theme"
        );
    }

    #[test]
    fn transient_notice_is_right_aligned_on_the_composer_bottom_rule() {
        let editor = Editor::default();
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &[],
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: Some("• Copied to clipboard".to_owned()),
                composer_mode: None,
            },
            80,
        );

        let notice = frame
            .lines
            .iter()
            .find(|line| {
                line.tail
                    .iter()
                    .any(|span| span.text == "• Copied to clipboard")
            })
            .expect("notice row");
        assert_eq!(
            painted(notice),
            format!("╰{}  • Copied to clipboard ─╯", "─".repeat(52))
        );
    }

    #[test]
    fn a_scrolling_window_always_contains_the_selection() {
        // Shorter than the window: everything shows, wherever the selection is.
        assert_eq!(visible_window(Some(0), 4, 6), 0..4);
        assert_eq!(visible_window(Some(3), 4, 6), 0..4);
        assert_eq!(visible_window(None, 4, 6), 0..4);
        assert_eq!(visible_window(Some(0), 0, 6), 0..0);

        // Longer: the window holds still until the selection would leave it,
        // and it stays full at the end of the list instead of shrinking.
        assert_eq!(visible_window(Some(0), 12, 6), 0..6);
        assert_eq!(visible_window(Some(2), 12, 6), 0..6);
        assert_eq!(visible_window(Some(3), 12, 6), 1..7);
        assert_eq!(visible_window(Some(11), 12, 6), 6..12);

        for len in [1usize, 5, 6, 7, 20, 100] {
            for rows in [1usize, 6, 9] {
                for selected in 0..len {
                    let window = visible_window(Some(selected), len, rows);
                    assert!(
                        window.contains(&selected),
                        "len {len} rows {rows}: {selected} outside {window:?}"
                    );
                    assert_eq!(window.len(), rows.min(len), "len {len} rows {rows}");
                    assert!(window.end <= len);
                }
            }
        }
    }

    #[test]
    fn the_command_dock_scrolls_to_keep_the_selection_visible() {
        let dock = |selected: usize| {
            (0..12)
                .map(|index| SuggestionView {
                    command: format!("/cmd{index}"),
                    description: "does a thing".to_owned(),
                    selected: index == selected,
                    category: None,
                    panel_title: "Commands",
                    hint: None,
                })
                .collect::<Vec<_>>()
        };

        for selected in [0usize, 5, 9, 11] {
            let suggestions = dock(selected);
            let painted = suggestion_lines(&suggestions, 80)
                .iter()
                .map(painted)
                .collect::<Vec<_>>();

            let row = painted
                .iter()
                .find(|line| line.contains(&format!("/cmd{selected} ")))
                .unwrap_or_else(|| panic!("selection {selected} fell off the dock"));
            assert!(
                row.contains('❯'),
                "selection {selected} is on screen but unmarked"
            );
        }
    }

    /// A notice appearing must not change the frame height: it claims the spacer
    /// row above the composer rather than stacking on top of it.
    #[test]
    fn transient_notice_does_not_push_the_transcript_up_a_row() {
        let editor = Editor::default();
        let frame = |live: &[Block], notice: Option<&str>| {
            normal_frame(
                live,
                &editor,
                None,
                &[],
                None,
                StatusArea {
                    fallback: String::new(),
                    line: None,
                    composer_notice: notice.map(str::to_owned),
                    composer_mode: None,
                },
                80,
            )
        };

        // Both the inline case (live blocks above the composer) and the
        // fullscreen one, where the transcript has already been committed and
        // the live frame is nothing but the composer.
        let with_block = [Block::new(BlockKind::Assistant, "Codex", "done")];
        for live in [&[][..], &with_block[..]] {
            let bare = frame(live, None);
            let noticed = frame(live, Some("• Copied to clipboard"));

            assert_eq!(
                noticed.lines.len(),
                bare.lines.len(),
                "the notice added a row instead of taking the reserved one"
            );
            assert_eq!(
                noticed.cursor_line, bare.cursor_line,
                "the composer moved when the notice appeared"
            );
        }

        // A notice too long for the terminal folds rather than growing the frame.
        let long = "Copied ".repeat(40);
        assert_eq!(
            frame(&[], Some(&long)).lines.len(),
            frame(&[], None).lines.len(),
            "a long notice wrapped onto a second row"
        );
    }

    #[test]
    fn recalled_history_labels_the_composer_rule_with_its_position() {
        let mut editor = Editor::default();
        for prompt in ["first", "second"] {
            editor.set_text(prompt);
            editor.take_for_submit();
        }
        let status = || StatusArea {
            fallback: String::new(),
            line: None,
            composer_notice: None,
            composer_mode: None,
        };

        let bare = normal_frame(&[], &editor, None, &[], None, status(), 80);
        assert!(
            bare.lines
                .iter()
                .all(|line| !painted(line).starts_with("╭─ ")),
            "an unrecalled composer should carry no label"
        );

        editor.history_previous();
        let recalled = normal_frame(&[], &editor, None, &[], None, status(), 80);

        assert_eq!(editor.text(), "second");
        assert!(
            recalled
                .lines
                .iter()
                .any(|line| painted(line).starts_with("╭─ 2/2 ─")),
            "the composer rule should show the history position"
        );
    }

    /// Activity is pinned to the composer, so it stays visible while the model works.
    #[test]
    fn activity_sits_directly_above_the_composer_rule() {
        let editor = Editor::default();
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &[],
            Some("✶ Working (2s • esc to interrupt)"),
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        let activity = frame
            .lines
            .iter()
            .position(|line| painted_line_text(line).contains("Working"))
            .expect("activity row");
        assert_eq!(frame.lines[activity].tone, Tone::Plain);
        assert!(activity >= 1);
        assert!(frame.lines[activity - 1] == PaintLine::blank());
        assert!(painted(&frame.lines[activity + 1]).starts_with('╭'));
    }

    #[test]
    fn completed_activity_uses_the_spacer_below_it() {
        let editor = Editor::default();
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &[],
            Some("✧ Completed (1m 36s)"),
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        let activity = frame
            .lines
            .iter()
            .position(|line| painted_line_text(line).contains("Completed"))
            .expect("completion row");
        assert!(activity >= 1);
        assert!(frame.lines[activity - 1] == PaintLine::blank());
        assert!(painted(&frame.lines[activity + 1]).starts_with('╭'));
    }

    #[test]
    fn composer_controls_share_the_activity_row_when_they_fit() {
        let editor = Editor::default();
        let mode = test_mode("Full Access", ModeAccent::Danger, false);
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &[],
            Some("Completed GPT-5.6-Terra · high (10s)"),
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: Some(mode),
            },
            160,
        );

        let activity = frame
            .lines
            .iter()
            .position(|line| painted_line_text(line).contains("Completed"))
            .expect("activity row");
        assert!(!painted(&frame.lines[activity]).contains("View: Chat"));
        assert!(painted(&frame.lines[activity]).contains("Fast: Off"));
        assert!(!painted(&frame.lines[activity + 1]).contains("View: Chat"));
        assert_eq!(painted_width(&frame.lines[activity]), 158);
        assert_eq!(frame.lines[activity + 1].tone, Tone::ModelTerra);
    }

    #[test]
    fn composer_controls_compress_on_the_activity_row_when_it_is_too_narrow() {
        let editor = Editor::default();
        let mode = test_mode("Full Access", ModeAccent::Danger, false);
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &[],
            Some("Completed GPT-5.6-Terra · high (10s)"),
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: Some(mode),
            },
            80,
        );

        let activity = frame
            .lines
            .iter()
            .position(|line| painted_line_text(line).contains("Completed"))
            .expect("activity row");
        assert!(!painted(&frame.lines[activity]).contains("View: Chat"));
        assert!(!painted(&frame.lines[activity]).contains("Shell: Collapse"));
        assert!(!painted(&frame.lines[activity + 1]).contains("View: Chat"));
    }

    #[test]
    fn copy_notice_replaces_only_the_right_hand_controls() {
        let editor = Editor::default();
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.branch = Some("feature/copy-notice".to_owned());
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &[],
            Some("Working.. (2s)"),
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: Some("• Copied to clipboard".to_owned()),
                composer_mode: Some(mode),
            },
            120,
        );

        let notice = frame
            .lines
            .iter()
            .find(|line| painted_line_text(line).contains("Working.. (2s)"))
            .expect("activity row");
        assert!(painted(notice).contains("• Copied to clipboard"));
        assert!(!painted(notice).contains("feature/copy-notice"));
        assert!(!painted(notice).contains("Vibe: On"));
        assert!(!painted(notice).contains("Fast: On"));
        assert!(
            !frame
                .lines
                .iter()
                .filter(|line| painted_line_text(line).contains("• Copied to clipboard"))
                .any(|line| line != notice)
        );
    }

    #[test]
    fn next_request_notice_uses_the_copy_notice_location_while_working() {
        let editor = Editor::default();
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &[],
            Some("Working.. (2s)"),
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: Some("Applies to the next request".to_owned()),
                composer_mode: Some(mode),
            },
            120,
        );

        let activity = frame
            .lines
            .iter()
            .find(|line| painted_line_text(line).contains("Working.. (2s)"))
            .expect("activity row");
        assert!(painted(activity).contains("Applies to the next request"));
        assert!(frame.lines.iter().all(|line| {
            !painted(line).starts_with('╰')
                || !painted(line).contains("Applies to the next request")
        }));
    }

    #[test]
    fn working_activity_uses_its_model_tone() {
        let line = activity_lines("Working.. (2m 12s)", Some("gpt-5.6-terra"), 0.5, 80)
            .pop()
            .expect("working row");

        assert_eq!(line.prefix, " ");
        assert_eq!(line.text, "");
        assert_eq!(line.tail.first().map(|span| span.text.as_str()), Some("⠴ "));
        assert_eq!(
            line.tail.first().map(|span| span.tone),
            Some(Tone::ModelTerra)
        );
        assert_eq!(line.tone, Tone::ModelTerra);
        assert_eq!(
            line.tail
                .iter()
                .filter_map(|span| match span.tone {
                    Tone::Shimmer(_, _) => Some(span.text.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            "Working.. (2m 12s)"
        );
    }

    #[test]
    fn completed_activity_label_is_static() {
        let line = activity_lines("✧ Completed (2m 12s)", Some("gpt-5.6-terra"), 0.5, 80)
            .pop()
            .expect("completion row");

        assert_eq!(line.prefix, " ");
        assert_eq!(line.tone, Tone::ModelTerra);
        assert!(
            line.tail
                .iter()
                .all(|span| !matches!(span.tone, Tone::Shimmer(_, _))),
            "a completed turn must not look like it is still working"
        );
    }

    #[test]
    fn interrupted_activity_label_is_static() {
        let line = activity_lines("X Interrupted", Some("gpt-5.6-terra"), 0.5, 80)
            .pop()
            .expect("interrupted row");

        assert_eq!(line.text, "X ");
        assert!(
            line.tail
                .iter()
                .all(|span| !matches!(span.tone, Tone::Shimmer(_, _))),
            "an interrupted turn must not look like it is still working"
        );
    }

    #[test]
    fn copy_notice_activity_uses_plain_text() {
        let line = activity_lines("• Copied to clipboard", None, 0.5, 80)
            .pop()
            .expect("copy notice row");

        assert_eq!(line.tone, Tone::Plain);
        assert_eq!(line.text, "• ");
    }

    /// A row too narrow for the whole line wraps plainly rather than shimmering
    /// across two rows.
    #[test]
    fn a_cramped_activity_row_falls_back_to_plain_wrapping() {
        let lines = activity_lines("✶ Working (2s • esc to interrupt)", None, 0.5, 12);

        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .flat_map(|line| line.tail.iter())
                .all(|span| !matches!(span.tone, Tone::Shimmer(_, _)))
        );
    }

    fn rule_width(line: &PaintLine) -> usize {
        UnicodeWidthStr::width(line.text.as_str())
            + line
                .tail
                .iter()
                .map(|span| UnicodeWidthStr::width(span.text.as_str()))
                .sum::<usize>()
    }

    fn test_mode(label: &str, accent: ModeAccent, fast_mode: bool) -> ComposerMode {
        ComposerMode {
            branch: None,
            vibe_mode: "Vibe: On".to_owned(),
            vibe_tone: VibeTone::On,
            label: label.to_owned(),
            accent,
            model: "GPT-5.6-Terra".to_owned(),
            response_length: "Short".to_owned(),
            fast_mode,
            claude_permission: None,
            effort: "high".to_owned(),
            cost: None,
            shell_display_mode: "Collapse".to_owned(),
            diff_display_mode: "Collapse".to_owned(),
        }
    }

    #[test]
    fn composer_chrome_and_prompt_use_the_model_tone() {
        let editor = Editor::default();
        let mode = test_mode("Default", ModeAccent::Calm, false);

        let (rows, _, _, _) = input_lines(&editor, &[], 80, "", "Ask anything", None, Some(&mode));

        assert_eq!(rows[0].tone, Tone::ModelTerra);
        assert_eq!(rows[1].prefix_tone, Tone::ModelTerra);
        assert_eq!(rows[1].tone, Tone::ModelTerra);
        assert_eq!(
            rows[1].tail.last().map(|span| span.tone),
            Some(Tone::ModelTerra)
        );
        assert_eq!(rows.last().map(|line| line.tone), Some(Tone::ModelTerra));
    }

    #[test]
    fn claude_composer_chrome_uses_the_selected_model_tone() {
        let editor = Editor::default();
        let mut mode = test_mode("Default", ModeAccent::Calm, false);
        mode.model = "claude:opus[1m]".to_owned();

        let (rows, _, _, _) = input_lines(&editor, &[], 80, "", "Ask anything", None, Some(&mode));

        assert_eq!(rows[0].tone, Tone::ModelOpus);
        assert_eq!(rows[1].prefix_tone, Tone::ModelOpus);
        assert_eq!(rows.last().map(|line| line.tone), Some(Tone::ModelOpus));
    }

    #[test]
    fn queue_preview_is_one_line_and_truncates_the_prompt() {
        let line = queue_preview_line("a very long queued prompt", 0, 18);

        assert_eq!(painted(&line), " X Queue: a very…");
        assert_eq!(pick_on(&line, "X"), Some(Pick::RemoveQueuedPrompt(0)));
    }

    #[test]
    fn queue_preview_shows_every_prompt_in_fifo_order() {
        let prompts = ["first", "second", "third", "fourth"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let lines = queue_preview_lines(&prompts, 80);

        assert_eq!(
            lines.iter().map(painted).collect::<Vec<_>>(),
            [
                " X Queue: first",
                " X Queue: second",
                " X Queue: third",
                " X Queue: fourth"
            ]
        );
        assert_eq!(pick_on(&lines[3], "X"), Some(Pick::RemoveQueuedPrompt(3)));
    }

    fn test_subagent(name: &str, description: &str, tool: &str, secs: u64) -> SubagentView {
        SubagentView {
            id: format!("toolu_{name}"),
            name: name.to_owned(),
            description: description.to_owned(),
            tool: tool.to_owned(),
            elapsed: Duration::from_secs(secs),
        }
    }

    #[test]
    fn running_subagents_show_only_name_and_elapsed_time() {
        let subagents = [
            test_subagent("Explore", "Find auth code", "Grep(fn login)", 93),
            test_subagent("developer", "Fix the parser", "", 3),
        ];

        assert_eq!(
            subagent_lines(&subagents, 80)
                .iter()
                .map(painted)
                .collect::<Vec<_>>(),
            [" ⏺ Explore · 1m 33s", " ⏺ developer · 3s",]
        );
    }

    #[test]
    fn subagent_row_compacts_only_the_name() {
        let subagent = test_subagent(
            "very-long-agent-name",
            "hidden description",
            "Grep(hidden)",
            93,
        );

        let line = subagent_line(&subagent, 0, 30);

        assert!(!painted(&line).contains("hidden"));
        assert!(painted(&line).ends_with(" · 1m 33s"));
        assert!(painted_line_width(&line) <= 30);
    }

    #[test]
    fn a_subagent_row_opens_its_own_panel_but_its_elapsed_reading_does_not() {
        let lines = subagent_lines(
            &[
                test_subagent("Explore", "Find auth code", "", 4),
                test_subagent("developer", "Fix the parser", "", 1),
            ],
            80,
        );

        assert_eq!(pick_on(&lines[0], "⏺"), Some(Pick::Subagent(0)));
        assert_eq!(pick_on(&lines[0], "Explore"), Some(Pick::Subagent(0)));
        assert_eq!(pick_on(&lines[1], "developer"), Some(Pick::Subagent(1)));
        assert_eq!(pick_on(&lines[0], "4s"), None);
    }

    #[test]
    fn running_subagents_sit_below_the_status_line() {
        let editor = Editor::default();
        let frame = normal_frame_with_expansion(
            Vec::new(),
            &editor,
            &[],
            &[],
            &[test_subagent("Explore", "Find auth code", "", 4)],
            "",
            None,
            &[],
            None,
            None,
            0.5,
            0.5,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        let composer_index = frame.composer_index.expect("composer index");
        let subagent_index = frame
            .lines
            .iter()
            .position(|line| painted(line).contains("Explore"))
            .expect("subagent row");

        assert!(subagent_index > composer_index);
        assert_eq!(subagent_index, frame.lines.len() - 1);
    }

    #[test]
    fn composer_controls_sit_inside_the_composer_rule() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(120, "", Some(&mode));
        let texts = line
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rule_width(&line), 120);
        assert!(painted(&line).starts_with('╭'));
        // Two blanks off the rule, the badge, then the rule resumes for two columns.
        assert_eq!(texts, ["  ", "Vibe: On", " · ", "Fast: On", " ", "─╮"]);
        assert_eq!(line.tail[1].tone, Tone::FastOn);
        assert_eq!(line.tail[3].tone, Tone::FastOn);
    }

    #[test]
    fn claude_composer_hides_the_fast_control() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.model = "claude:sonnet".to_owned();

        let line = input_top_line(120, "", Some(&mode));

        assert!(!painted(&line).contains("Fast:"));
        assert!(
            line.pick
                .as_ref()
                .is_none_or(|picks| { picks.0.iter().all(|(_, _, pick)| *pick != Pick::FastMode) })
        );
    }

    /// The slot Fast holds on a Codex thread carries Claude's permission mode,
    /// painted in that mode's own colour and clickable like the other badges.
    #[test]
    fn claude_composer_shows_the_permission_mode_in_the_fast_slot() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.model = "claude:sonnet".to_owned();
        mode.claude_permission = Some(PermissionBadge {
            label: "⏸ plan mode".to_owned(),
            tone: PermissionTone::Plan,
        });

        let line = input_top_line(120, "", Some(&mode));

        assert!(painted(&line).contains("⏸ plan mode"));
        assert_eq!(
            pick_on(&line, "⏸ plan mode"),
            Some(Pick::ClaudePermissionMode)
        );
        assert!(
            line.tail
                .iter()
                .any(|span| span.tone == Tone::ClaudePlan && span.text.contains("plan mode"))
        );
    }

    #[test]
    fn idle_composer_controls_stay_above_the_composer_rule() {
        let editor = Editor::default();
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &[],
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: Some(test_mode("Full Access", ModeAccent::Danger, false)),
            },
            120,
        );
        let composer = frame.composer_index.expect("composer index");

        assert!(!painted(&frame.lines[composer - 1]).contains("View: Chat"));
        assert!(painted(&frame.lines[composer - 1]).contains("Fast: Off"));
        assert!(!painted(&frame.lines[composer]).contains("View: Chat"));
    }

    /// What the row answers with at the first column of `label`.
    fn pick_on(line: &PaintLine, label: &str) -> Option<Pick> {
        let text = painted(line);
        let start = text.find(label)?;
        line.pick
            .as_ref()?
            .at(UnicodeWidthStr::width(&text[..start]))
    }

    /// What the row answers with at the middle column of `label` — where a gap
    /// between two clickable spans is genuinely nobody's, the columns either side
    /// of it having been given away to the spans they touch.
    fn pick_mid(line: &PaintLine, label: &str) -> Option<Pick> {
        let text = painted(line);
        let start = text.find(label)?;
        let column = UnicodeWidthStr::width(&text[..start]) + UnicodeWidthStr::width(label) / 2;
        line.pick.as_ref()?.at(column)
    }

    #[test]
    fn composer_badges_answer_to_their_own_columns() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(120, "", Some(&mode));
        assert_eq!(pick_on(&line, "View: Chat"), None);
        assert_eq!(pick_on(&line, "Vibe: On"), Some(Pick::VibeMode));
        assert_eq!(pick_on(&line, "Fast: On"), Some(Pick::FastMode));
        // The rule, and the middle of the separator between the badges, are not
        // settings — the columns beside each badge belong to that badge.
        assert_eq!(pick_mid(&line, " · "), None);
        assert_eq!(line.pick.as_ref().unwrap().at(0), None);
    }

    #[test]
    fn the_view_badge_is_clickable_without_an_access_badge() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(80, "", Some(&mode));

        assert!(!painted(&line).contains("Full Access"));
        assert_eq!(pick_on(&line, "View: Chat"), None);
    }

    /// The cost pushes both badges right and a recalled-history label pushes the
    /// whole rule along with them, so the columns are only ever read off the
    /// spans as painted.
    #[test]
    fn the_cost_and_a_rule_label_do_not_move_the_badges_out_from_under_the_click() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());
        let line = input_top_line(120, "3/12", Some(&mode));

        assert_eq!(pick_on(&line, "Vibe: On"), Some(Pick::VibeMode));
        assert_eq!(pick_on(&line, "Fast: On"), Some(Pick::FastMode));
        assert_eq!(pick_on(&line, "[$0.95]"), None);
        assert_eq!(pick_on(&line, "3/12"), None);
    }

    /// Removing View leaves enough room for Fast beside the Vibe control.
    #[test]
    fn removed_view_keeps_the_fast_flag_clickable() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(40, "", Some(&mode));

        assert!(painted(&line).contains("Fast: On"));
        assert_eq!(pick_on(&line, "Vibe: On"), Some(Pick::VibeMode));
        assert_eq!(pick_on(&line, "Fast: On"), Some(Pick::FastMode));
    }

    #[test]
    fn compression_is_retained_before_fast_off_at_narrow_width() {
        let mode = test_mode("Default", ModeAccent::Safe, false);
        let line = input_top_line(80, "", Some(&mode));
        let texts = line
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rule_width(&line), 80);
        assert_eq!(texts, ["  ", "Vibe: On", " · ", "Fast: Off", " ", "─╮"]);
        assert_eq!(line.tail[3].tone, Tone::FastOff);
    }

    #[test]
    fn estimated_cost_is_not_shown_above_the_composer() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());
        let line = input_top_line(80, "", Some(&mode));
        assert_eq!(rule_width(&line), 80);
        assert!(!painted(&line).contains("$0.95"));
        assert_eq!(pick_on(&line, "Vibe: On"), Some(Pick::VibeMode));
    }

    #[test]
    fn estimated_cost_is_excluded_from_composer_badges() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());

        let line = input_top_line(80, "", Some(&mode));

        assert!(!painted(&line).contains("$0.95"));
    }

    #[test]
    fn composer_rule_places_branch_before_the_display_badges_without_cost() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, false);
        mode.branch = Some("main".to_owned());
        mode.cost = Some("$0.95".to_owned());

        let line = input_top_line(120, "", Some(&mode));

        assert!(painted(&line).contains("* main | Vibe: On"));
        assert!(!painted(&line).contains("$0.95"));
    }

    #[test]
    fn vibe_badge_is_clickable_outside_custom_mode() {
        let line = input_top_line(
            120,
            "",
            Some(&test_mode("Full Access", ModeAccent::Danger, false)),
        );

        assert_eq!(pick_on(&line, "Vibe: On"), Some(Pick::VibeMode));
    }

    #[test]
    fn vibe_and_fast_badges_use_their_role_tones() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        let tones = |mode: &ComposerMode| {
            fitting_badge_spans(mode, 120)
                .unwrap()
                .spans
                .into_iter()
                .map(|span| (span.text, span.tone))
                .collect::<Vec<_>>()
        };

        let on = tones(&mode);
        assert!(on.contains(&("Vibe: On".to_owned(), Tone::FastOn)));
        assert!(!on.iter().any(|(text, _)| text == "View: Chat"));
        assert!(on.contains(&("Fast: On".to_owned(), Tone::FastOn)));

        mode.vibe_tone = VibeTone::Off;
        assert!(tones(&mode).contains(&("Vibe: On".to_owned(), Tone::Muted)));

        mode.vibe_tone = VibeTone::Super;
        assert!(tones(&mode).contains(&("Vibe: On".to_owned(), Tone::VibeSuper)));
    }

    #[test]
    fn vibe_picker_steps_use_the_composer_vibe_tones() {
        let expected = [Tone::Muted, Tone::FastOn, Tone::VibeSuper];

        for (selected, tone) in expected.into_iter().enumerate() {
            let slider = EffortSlider {
                efforts: ["Off", "On", "Super Vibe"].map(ToOwned::to_owned).to_vec(),
                selected,
                detail: None,
            };
            let lines = effort_step_lines(&slider, 80);
            assert_eq!(lines[0].tone, tone);
            assert_eq!(lines[2].tone, tone);
            assert!(
                lines[1]
                    .tail
                    .iter()
                    .any(|span| span.bold && span.tone == tone)
            );
        }
    }

    /// Shell and diff readings live behind the vibe preset now, so the rule
    /// carries the view and the preset instead of one badge per reading.
    #[test]
    fn display_badges_answer_to_the_view_and_the_vibe_preset() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());
        let line = input_top_line(80, "", Some(&mode));

        assert_eq!(pick_on(&line, "View: Chat"), None);
        assert_eq!(pick_on(&line, "Vibe: On"), Some(Pick::VibeMode));
        assert_eq!(pick_on(&line, "Shell: Collapse"), None);
    }

    #[test]
    fn hidden_cost_leaves_room_for_fast_control() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());
        let line = input_top_line(66, "", Some(&mode));
        assert_eq!(rule_width(&line), 66);
        assert!(!painted(&line).contains("$0.95"));
        assert!(painted(&line).contains("Vibe: On"));
        assert!(painted(&line).contains("Fast: On"));
    }

    #[test]
    fn tight_composer_rule_keeps_the_mode_and_fast_flag_after_view_removal() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(40, "", Some(&mode));
        let texts = line
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rule_width(&line), 40);
        assert_eq!(texts, ["  ", "Vibe: On", " · ", "Fast: On", " ", "─╮"]);
    }

    #[test]
    fn narrow_composer_rule_drops_the_badge_instead_of_ellipsizing() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(9, "", Some(&mode));

        assert!(line.tail.is_empty());
        assert_eq!(rule_width(&line), 9);
    }

    #[test]
    fn slash_suggestions_dock_directly_above_the_composer() {
        let mut editor = Editor::default();
        editor.set_text("/");
        let suggestions = vec![SuggestionView {
            command: "/model".to_owned(),
            description: "Switch model".to_owned(),
            selected: true,
            category: None,
            panel_title: "Commands",
            hint: None,
        }];
        let mut frame = normal_frame(
            &[],
            &editor,
            None,
            &suggestions,
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        fit_frame(&mut frame, 20);

        let suggestion_end = frame
            .lines
            .iter()
            .position(|line| line.text.starts_with('╰'))
            .expect("suggestion panel bottom");
        // One blank row keeps the panel off the composer rule; nothing else may
        // come between them.
        assert!(frame.lines[suggestion_end + 1] == PaintLine::blank());
        let rule = &frame.lines[suggestion_end + 2];
        assert!(painted(rule).starts_with('╭'));
    }

    /// Whatever the composer is docked under, the row directly above its rule is
    /// blank — the transient notice is the only thing allowed to fill it.
    #[test]
    fn the_row_above_the_composer_stays_blank() {
        let editor = Editor::default();
        let live = vec![Block::new(BlockKind::Assistant, "Codex", "done")];
        let frame = normal_frame(
            &live,
            &editor,
            None,
            &[],
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        let rule = frame
            .lines
            .iter()
            .position(|line| painted(line).starts_with('╭'))
            .expect("composer rule");
        assert!(rule > 0);
        assert!(frame.lines[rule - 1] == PaintLine::blank());
        assert!(
            frame.lines[rule - 2] != PaintLine::blank(),
            "exactly one blank row, not a growing gap"
        );
    }

    #[test]
    fn activity_sits_above_slash_suggestions() {
        let mut editor = Editor::default();
        editor.set_text("/");
        let suggestions = vec![SuggestionView {
            command: "/model".to_owned(),
            description: "Switch model".to_owned(),
            selected: true,
            category: None,
            panel_title: "Commands",
            hint: None,
        }];
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &suggestions,
            Some("✶ Working"),
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        let activity = frame
            .lines
            .iter()
            .position(|line| painted_line_text(line).contains("Working"))
            .expect("activity row");
        let suggestions = frame
            .lines
            .iter()
            .position(|line| line.text == "Commands ")
            .expect("suggestions header");

        assert!(activity > 0);
        assert!(frame.lines[activity - 1] == PaintLine::blank());
        assert!(frame.lines[activity + 1] == PaintLine::blank());
        assert!(activity < suggestions);
    }

    #[test]
    fn conversation_blocks_hide_speaker_labels() {
        let user = Block::new(BlockKind::User, "You", "hello");
        let assistant = Block::new(BlockKind::Assistant, "Codex", "hi");

        let user_lines = block_lines(&user, 80);
        let assistant_lines = block_lines(&assistant, 80);

        // The prompt keeps its `› ` marker; only the speaker label is dropped.
        assert_eq!(user_lines[1].prefix, format!("{}› ", " ".repeat(70)));
        assert_eq!(user_lines[1].text, "hello  ");
        assert!(user_lines[1].prefix_tone == Tone::User);
        assert!(user_lines[1].tone == Tone::UserPrompt);
        assert!(!user_lines[1].bold);
        assert_eq!(assistant_lines[1].prefix, "  ");
        assert_eq!(assistant_lines[1].prefix_tone, Tone::FastOff);
        assert_eq!(assistant_lines[1].text, "hi");
        assert_eq!(assistant_lines[1].tone, Tone::AssistantBubble);
        assert!(user_lines.iter().all(|line| line.text != "You"));
        assert!(assistant_lines.iter().all(|line| line.text != "Codex"));
    }

    #[test]
    fn chat_assistant_bubble_covers_marker_and_highlight_spans() {
        CHAT_LAYOUT.store(true, Ordering::Relaxed);
        let assistant = Block::new(BlockKind::Assistant, "Codex", "`highlight`");
        let line = block_lines(&assistant, 80)
            .into_iter()
            .find(|line| line.text.contains("highlight"))
            .expect("assistant content row");
        let mut frame = CellFrame::new(80, 1);

        paint_line_into_frame(&mut frame, 0, &line, None, None, None);

        let background = Some(assistant_bubble_background());
        assert_eq!(frame.cell(0, 0).style.background, background);
        assert_eq!(frame.cell(2, 0).style.background, background);
    }

    #[test]
    fn transcript_gutters_use_the_theme_accent() {
        // The assistant gutter uses the same circular mark as a response's first row.
        for (block, tone) in [
            (
                Block::new(BlockKind::Assistant, "Codex", "answer"),
                Tone::FastOff,
            ),
            (Block::new(BlockKind::Tool, "Shell", "output"), Tone::Accent),
            (
                Block::new(
                    BlockKind::FileChange,
                    "Update(src/main.rs)",
                    "Added 1 · Removed 0",
                ),
                Tone::Accent,
            ),
            (Block::new(BlockKind::Diff, "Diff", "changed"), Tone::Accent),
        ] {
            let line = block_lines(&block, 80)
                .into_iter()
                .find(|line| matches!(line.prefix.as_str(), "  " | "● "))
                .expect("transcript gutter");
            assert_eq!(line.prefix_tone, tone);
        }
    }

    #[test]
    fn context_compaction_uses_the_theme_accent_and_circular_gutter() {
        let lines = block_lines(&Block::new(BlockKind::System, "Context compacted", ""), 80);

        assert_eq!(lines[0].prefix, "● ");
        assert_eq!(lines[0].prefix_tone, Tone::Accent);
        assert_eq!(lines[0].tone, Tone::Accent);
        assert_eq!(lines[0].text, "Context compacted");
    }

    #[test]
    fn live_output_groups_have_exactly_one_blank_row_between_them() {
        let editor = Editor::default();
        let live = vec![
            Block::new(BlockKind::Reasoning, "Thinking…", "working"),
            Block::new(BlockKind::Tool, "Shell · first", ""),
            Block::new(BlockKind::Tool, "Shell · second", ""),
            Block::new(BlockKind::Assistant, "Codex", "done"),
        ];
        let frame = normal_frame(
            &live,
            &editor,
            None,
            &[],
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        let rows = frame.lines[..8]
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                "working",
                "",
                "Shell · first",
                "",
                "Shell · second",
                "",
                // The response opens its chat bubble before its first text row.
                "▄▄▄▄▄▄▄▄",
                "done"
            ]
        );
    }

    fn text_rows(count: usize, label: &str) -> Vec<PaintLine> {
        (0..count)
            .map(|n| PaintLine {
                prefix: String::new(),
                prefix_tone: Tone::Plain,
                text: format!("{label}{n}"),
                tone: Tone::Plain,
                bold: false,
                tool_heading: None,
                pick: None,
                tail: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn plan_gap_skips_viewport_separator_rows() {
        let transcript = vec![
            PaintLine::blank(),
            PaintLine::blank(),
            PaintLine::plain("first visible row"),
            PaintLine::plain("second visible row"),
        ];
        let start = transcript_start_below_plan(&transcript, 0);
        let (screen, _) = compose_screen(&transcript, text_rows(1, "composer"), 2, start, 0);

        assert_eq!(start, 2);
        assert_eq!(
            screen.iter().map(painted).collect::<Vec<_>>(),
            ["first visible row", "second visible row", "composer0"]
        );
    }

    #[test]
    fn waiting_pulse_targets_only_the_latest_visible_response_bullet() {
        let response = |text: &str| PaintLine {
            prefix: RESPONSE_BULLET_PREFIX.to_owned(),
            prefix_tone: Tone::FastOff,
            text: text.to_owned(),
            tone: Tone::Plain,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        };
        let transcript = vec![
            response("older"),
            PaintLine::blank(),
            response("latest"),
            PaintLine::blank(),
        ];

        assert_eq!(visible_response_bullet_row(&transcript, 2..4, 3), Some(3));
        assert_eq!(visible_response_bullet_row(&transcript, 0..2, 0), None);
        assert!(matches!(
            waiting_response_bullet_tone(0.0),
            Tone::Shimmer(_, 0)
        ));
        assert!(matches!(
            waiting_response_bullet_tone(0.5),
            Tone::Shimmer(_, 255)
        ));
    }

    #[test]
    fn the_composer_holds_the_bottom_rows_at_every_scroll_position() {
        let transcript = text_rows(100, "t");
        let live = text_rows(3, "live");
        let rows = 20;
        let view_rows = rows - live.len();

        // Newest end, mid-history and oldest end: the live frame is the last
        // three rows of the screen in all of them, which is the whole point.
        for start in [0, 40, transcript.len() - view_rows] {
            let (screen, cursor_line) =
                compose_screen(&transcript, live.clone(), view_rows, start, 1);

            assert_eq!(screen.len(), rows);
            let painted = screen[view_rows..]
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>();
            assert_eq!(painted, ["live0", "live1", "live2"]);
            assert_eq!(cursor_line, view_rows + 1);
            // ...and the transcript above it is the window we asked for.
            assert_eq!(screen[0].text, format!("t{start}"));
        }
    }

    #[test]
    fn the_slack_goes_to_the_live_frame_not_above_it() {
        // A short transcript must not hand its leftover rows to the top of the
        // screen: that would float the whole live frame — welcome card included —
        // down onto the composer. The live frame absorbs them instead.
        assert_eq!(split_rows(30, 10, 2), (2, 28));
        assert_eq!(split_rows(30, 10, 0), (0, 30));
        // Once the transcript can fill its share, the split stops moving.
        assert_eq!(split_rows(30, 10, 100), (20, 10));
        // A live frame taller than the screen takes all of it and gets trimmed.
        assert_eq!(split_rows(20, 40, 100), (0, 20));
    }

    #[test]
    fn history_expansion_keeps_the_last_transcript_line_on_its_previous_row() {
        let rows = 30;
        let live_rows = 10;
        let collapsed_rows = 4;
        let expanded_rows = 16;
        let anchor = split_rows(rows, live_rows, collapsed_rows).0;

        assert_eq!(anchor, collapsed_rows);
        assert_eq!(
            split_rows_with_transcript_anchor(rows, live_rows, expanded_rows, Some(anchor)),
            (collapsed_rows, rows - collapsed_rows)
        );

        let transcript = text_rows(expanded_rows, "history");
        let start = transcript.len() - anchor;
        let (screen, _) = compose_screen(
            &transcript,
            text_rows(rows - anchor, "live"),
            anchor,
            start,
            0,
        );
        assert_eq!(screen[anchor - 1].text, "history15");
    }

    #[test]
    fn prompt_hosted_history_keeps_the_clicked_row_fixed_when_toggled() {
        let collapsed_len = 30;
        let collapsed_view_rows = 10;
        let anchored_start = 20;
        let clicked_transcript_row = 22;
        let clicked_screen_row = clicked_transcript_row - anchored_start;

        let expanded_len = 38;
        let expanded_view_rows = 15;
        let expanded_back =
            scroll_back_for_transcript_start(expanded_len, expanded_view_rows, anchored_start);
        let expanded_start = expanded_len - expanded_view_rows - expanded_back;
        assert_eq!(clicked_transcript_row - expanded_start, clicked_screen_row);

        let collapsed_back =
            scroll_back_for_transcript_start(collapsed_len, collapsed_view_rows, anchored_start);
        let collapsed_start = collapsed_len - collapsed_view_rows - collapsed_back;
        assert_eq!(clicked_transcript_row - collapsed_start, clicked_screen_row);
    }

    #[test]
    fn completed_answer_stays_on_the_streaming_row() {
        let rows = 8;
        let older = text_rows(12, "old");
        let answer = PaintLine::plain("마지막 답변");
        let activity = PaintLine::plain("상태");
        let composer = PaintLine::plain("입력창");
        let screen_row = |wrapped: &[PaintLine], mut frame: Frame| {
            let (view_rows, live_rows) = split_rows(rows, frame.lines.len(), wrapped.len());
            fit_frame(&mut frame, live_rows);
            let start = wrapped.len().saturating_sub(view_rows);
            compose_screen(wrapped, frame.lines, view_rows, start, frame.cursor_line)
                .0
                .iter()
                .position(|line| line.text == "마지막 답변")
                .expect("answer row")
        };
        let streaming = Frame {
            lines: vec![
                answer.clone(),
                PaintLine::blank(),
                activity.clone(),
                composer.clone(),
            ],
            cursor_line: 3,
            cursor_col: 0,
            show_cursor: true,
            dock_index: 2,
            composer_index: Some(3),
            composer_layout: None,
            activity_index: Some(2),
        };
        let streaming_row = screen_row(&older, streaming);

        let mut completed_transcript = older;
        completed_transcript.extend([answer, PaintLine::blank()]);
        let completed_frame = || Frame {
            lines: vec![PaintLine::blank(), activity.clone(), composer.clone()],
            cursor_line: 2,
            cursor_col: 0,
            show_cursor: true,
            dock_index: 0,
            composer_index: Some(2),
            composer_layout: None,
            activity_index: Some(1),
        };
        let shifted_row = screen_row(&completed_transcript, completed_frame());
        assert_eq!(shifted_row + 1, streaming_row);

        let mut stabilized = completed_frame();
        assert!(stabilized.absorb_leading_spacer());
        assert_eq!(screen_row(&completed_transcript, stabilized), streaming_row);
    }

    #[test]
    fn the_welcome_card_stays_at_the_top_while_the_composer_reaches_the_bottom() {
        // `dock_index` is what `fit_frame` pads at, so the welcome row above it
        // holds still while the composer below it is pushed to the last row.
        let mut frame = Frame {
            lines: text_rows(1, "welcome")
                .into_iter()
                .chain(text_rows(2, "composer"))
                .collect(),
            cursor_line: 1,
            cursor_col: 0,
            show_cursor: true,
            dock_index: 1,
            composer_index: Some(1),
            composer_layout: None,
            activity_index: None,
        };
        let (view_rows, live_rows) = split_rows(10, frame.lines.len(), 0);
        fit_frame(&mut frame, live_rows);
        let (screen, cursor_line) =
            compose_screen(&[], frame.lines, view_rows, 0, frame.cursor_line);

        assert_eq!(screen.len(), 10);
        assert_eq!(screen[0].text, "welcome0");
        assert!(screen[1..8].iter().all(|line| *line == PaintLine::blank()));
        assert_eq!(screen[8].text, "composer0");
        assert_eq!(screen[9].text, "composer1");
        assert_eq!(cursor_line, 8);
    }

    #[test]
    fn scrolling_reports_movement_and_stops_at_both_ends() {
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.wrapped = text_rows(30, "t");

        assert!(renderer.scroll(10));
        assert_eq!(renderer.scroll_back, 10);
        // Past the oldest row it clamps, and a wheel spun at the end is a no-op
        // so the caller can skip the repaint.
        assert!(renderer.scroll(100));
        assert_eq!(renderer.scroll_back, 30);
        assert!(!renderer.scroll(5));
        assert!(renderer.scroll(-100));
        assert_eq!(renderer.scroll_back, 0);
        assert!(!renderer.scroll(-1));
    }

    #[test]
    fn page_scroll_uses_the_visible_transcript_height() {
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.last_height = 40;
        renderer.last_transcript_rows = 8;
        renderer.wrapped = text_rows(80, "t");
        renderer.scroll_back = 24;

        let page = renderer.page_rows();
        assert_eq!(page, 8);
        assert!(renderer.scroll(-page));
        assert_eq!(renderer.scroll_back, 16);
    }

    #[test]
    fn prompt_jump_places_the_selected_block_at_the_top() {
        let prompt = Block::new(BlockKind::User, "Codex", "selected prompt");
        let prompt_id = prompt.id();
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.history = std::iter::once(prompt)
            .chain(transcript_rows(12, "after"))
            .collect();
        renderer.last_width = 80;
        renderer.last_transcript_rows = 4;
        renderer.rewrap(80);

        assert!(renderer.scroll_to_prompt(prompt_id));
        let start = renderer
            .wrapped
            .len()
            .saturating_sub(renderer.last_transcript_rows)
            .saturating_sub(renderer.scroll_back);
        assert_eq!(start, 0);
        assert!(!renderer.scroll_to_prompt(u64::MAX));
    }

    #[test]
    fn fullscreen_scroll_to_bottom_returns_to_the_latest_transcript() {
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.scroll_back = 8;

        assert!(renderer.scroll_to_bottom());
        assert_eq!(renderer.scroll_back, 0);
    }

    #[test]
    fn live_frame_cache_reuses_wrapping_and_caps_the_copied_tail() {
        let block = Block::new(BlockKind::System, "stream", "line\n".repeat(10_000));
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        let live = [LiveBlockView {
            block: &block,
            revision: 1,
        }];

        let first = renderer.live_frame_lines(&live, 80, 20);
        let cached_rows = renderer
            .live_frame_cache
            .as_ref()
            .expect("cache")
            .lines
            .len();
        let second = renderer.live_frame_lines(&live, 80, 20);

        assert!(cached_rows > first.len());
        assert_eq!(first.len(), 20);
        assert!(second == first);
        for _ in 0..10_000 {
            renderer.live_frame_lines(&live, 80, 20);
        }
        assert_eq!(renderer.live_cache_rebuilds, 1);
        assert_eq!(
            renderer.live_frame_cache.as_ref().expect("cache").blocks,
            vec![(block.id(), 1)]
        );

        let revised = [LiveBlockView {
            block: &block,
            revision: 2,
        }];
        renderer.live_frame_lines(&revised, 80, 20);
        assert_eq!(renderer.live_cache_rebuilds, 2);
        assert_eq!(
            renderer.live_frame_cache.as_ref().expect("cache").blocks,
            vec![(block.id(), 2)]
        );
    }

    #[test]
    fn inline_scroll_to_bottom_leaves_terminal_scrollback_alone() {
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Inline);
        renderer.scroll_back = 8;

        assert!(!renderer.scroll_to_bottom());
        assert_eq!(renderer.scroll_back, 8);
    }

    #[test]
    fn scroll_to_bottom_control_appears_only_while_reading_fullscreen_history() {
        theme::set_current(ThemeKind::Dark);
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        assert!(renderer.scroll_to_bottom_control(80).is_none());

        renderer.scroll_back = 1;
        let control = renderer
            .scroll_to_bottom_control(80)
            .expect("control while scrolled back");
        assert_eq!(
            UnicodeWidthStr::width(control.prefix.as_str()),
            (80 - UnicodeWidthStr::width(" Scroll to bottom (Ctrl+↓) ")) / 2
        );
        assert_eq!(control.text, " Scroll to bottom (Ctrl+↓) ");
        assert_eq!(control.tone, Tone::ScrollToBottom);
        assert_eq!(tone_rgb(control.tone), Some(theme::palette().foreground));
        assert_eq!(word_background(control.tone), None);
        assert_eq!(
            pick_on(&control, "Scroll to bottom"),
            Some(Pick::ScrollToBottom)
        );
        assert_eq!(
            Renderer::hover_columns(&control, None, Some(&Pick::ScrollToBottom)),
            Some(
                UnicodeWidthStr::width(control.prefix.as_str())
                    ..UnicodeWidthStr::width(control.prefix.as_str())
                        + UnicodeWidthStr::width(control.text.as_str())
            )
        );
        let start = UnicodeWidthStr::width(control.prefix.as_str());
        let mut frame = CellFrame::new(80, 1);
        paint_scroll_to_bottom_into_frame(&mut frame, 0, &control, false);
        assert_eq!(
            frame.cell(start, 0).style.background,
            Some(theme::palette().hover_bg)
        );

        paint_scroll_to_bottom_into_frame(&mut frame, 0, &control, true);
        assert_eq!(
            frame.cell(start, 0).style.background,
            Some(scroll_to_bottom_background(true))
        );
        assert_ne!(scroll_to_bottom_background(true), theme::palette().hover_bg);
        assert_eq!(scroll_to_bottom_overlay_row(12, Some(7)), Some(16));

        renderer.scroll_to_bottom();
        assert!(renderer.scroll_to_bottom_control(80).is_none());
    }

    #[test]
    fn overlay_pick_keeps_its_real_column_when_the_transcript_line_is_shorter() {
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        let mut line = PaintLine::plain("short");
        line.pick = Some(PickRegions::span(10, 20, Pick::ScrollToBottom));
        renderer.previous_lines = vec![line];

        assert!(renderer.begin_selection(12, 0));
        assert_eq!(
            renderer.finish_selection(12, 0),
            SelectionResult::Click(12, 0)
        );
        assert_eq!(renderer.pick_at(12, 0), Some(Pick::ScrollToBottom));
    }

    #[test]
    fn the_inline_renderer_leaves_scrolling_to_the_terminal() {
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Inline);
        renderer.wrapped = text_rows(30, "t");

        assert!(!renderer.scroll(10));
        assert_eq!(renderer.scroll_back, 0);
    }

    #[test]
    fn render_mode_parses_its_aliases_and_rejects_the_rest() {
        for value in ["fullscreen", "FULL", " alt ", "pinned"] {
            assert_eq!(RenderMode::parse(value), Some(RenderMode::Fullscreen));
        }
        for value in ["inline", "classic", "default", "main"] {
            assert_eq!(RenderMode::parse(value), Some(RenderMode::Inline));
        }
        assert_eq!(RenderMode::parse("windowed"), None);
    }

    #[test]
    fn non_bash_tool_output_keeps_the_last_five_rows_and_counts_the_rest() {
        let body = (1..=12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = block_lines(
            &Block::new(BlockKind::Tool, "MCP · server › tool", body),
            200,
        );

        let texts = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        // The tail is what matters: an exit message is printed last, not first.
        assert_eq!(
            texts,
            [
                "MCP · server › tool",
                "line 8",
                "line 9",
                "line 10",
                "line 11",
                "line 12",
                "… +7 lines"
            ]
        );
        assert_eq!(lines[6].tone, Tone::Muted);
    }

    #[test]
    fn one_long_tool_output_row_cannot_outgrow_the_row_budget() {
        let body = format!("first\nsecond\n{}", "x".repeat(400));
        let lines = block_lines(
            &Block::new(BlockKind::Tool, "MCP · server › tool", body),
            40,
        );

        // Two short rows plus three rows of the wrapped one: five painted rows,
        // never the nine-odd rows the long line would occupy on its own.
        assert_eq!(lines.len(), 1 + TOOL_OUTPUT_ROWS + 1);
        assert_eq!(lines[1].text, "first");
        assert_eq!(lines[2].text, "second");
        // The clipped row stays counted as hidden — most of it never showed up.
        assert_eq!(lines[6].text, "… +1 lines");
    }

    #[test]
    fn blank_tool_output_rows_do_not_spend_the_row_budget() {
        let body = "one\n\n\ntwo\n\nthree\n\n";
        let lines = block_lines(
            &Block::new(BlockKind::Tool, "MCP · server › tool", body),
            200,
        );

        let texts = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(texts, ["MCP · server › tool", "one", "two", "three"]);
    }

    #[test]
    fn short_tool_output_is_shown_whole_without_a_count() {
        let lines = block_lines(
            &Block::new(BlockKind::Tool, "MCP · server › tool", "/src\n"),
            200,
        );

        let texts = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(texts, ["MCP · server › tool", "/src"]);
    }

    #[test]
    fn bash_output_is_collapsed_to_its_heading_by_default() {
        let block = Block::new(
            BlockKind::Tool,
            "Shell · rg TODO · exit 0 · 12ms",
            "one\ntwo",
        );
        let lines = block_lines(&block, 200);

        // Only the heading; the blank that separates groups is added by the frame.
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].prefix, "▸ ");
        assert_eq!(lines[0].text, "Shell · rg TODO · exit 0 · 12ms");
        assert_eq!(lines[0].tool_heading, Some(block.id()));
    }

    #[test]
    fn expanded_bash_output_shows_every_non_empty_row() {
        let block = Block::new(BlockKind::Tool, "Shell · rg TODO", "one\n\n two\nthree");
        let lines = block_lines_with_expansion(&block, 200, true);
        let texts = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(texts, ["Shell · rg TODO", "one", " two", "three"]);
        assert_eq!(lines[0].prefix, "▾ ");
    }

    #[test]
    fn collapsed_shell_group_is_one_clickable_row() {
        let group = Block::shell_group(
            BlockKind::Tool,
            "Shell · 2 commands · all passed · 1.2s",
            vec![
                Block::new(BlockKind::Tool, "Shell · first · exit 0", "one"),
                Block::new(BlockKind::Tool, "Shell · second · exit 0", "two"),
            ],
        );

        let lines = block_lines(&group, 80);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Shell · 2 commands · all passed · 1.2s");
        assert_eq!(lines[0].tool_heading, Some(group.id()));
    }

    #[test]
    fn hidden_shell_group_paints_no_rows() {
        let group = Block::shell_group(
            BlockKind::Tool,
            "Shell · 2 commands · all passed",
            vec![
                Block::new(BlockKind::Tool, "Shell · first · exit 0", "one"),
                Block::new(BlockKind::Tool, "Shell · second · exit 0", "two"),
            ],
        );

        assert!(
            shell_group_lines(&group, 80, crate::state::ShellDisplayMode::Hide, false).is_empty()
        );
    }

    #[test]
    fn hide_omits_shell_web_search_and_auxiliary_tool_blocks() {
        for hidden in [
            Block::new(BlockKind::Tool, "Running 1 shell command", ""),
            Block::new(BlockKind::Tool, "Running Shell Command", ""),
            Block::new(BlockKind::Warning, "Running 2 Shell Commands", ""),
            Block::new(BlockKind::System, "Running Shell Command", ""),
            Block::new(BlockKind::Tool, "Command", "Running Shell Command"),
            Block::new(BlockKind::Tool, "Web search", ""),
            Block::new(BlockKind::Tool, "Web search · rust ownership", ""),
            Block::new(BlockKind::Tool, "MCP · node_repl › js", "2"),
            Block::new(BlockKind::Tool, "MCP · docs › search", "result"),
            Block::new(BlockKind::Tool, "Tool · lookup", "result"),
            Block::new(BlockKind::Tool, "Agent", "result"),
        ] {
            assert!(
                visible_transcript_blocks(
                    &[hidden],
                    ShellDisplayMode::Hide,
                    DiffDisplayMode::Collapse
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn empty_thinking_placeholders_are_not_visible_transcript_blocks() {
        let blocks = vec![
            Block::new(BlockKind::Reasoning, THINKING_TITLE, ""),
            Block::new(BlockKind::Reasoning, THINKING_TITLE, "actual summary"),
        ];

        let visible = visible_transcript_blocks(
            &blocks,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Collapse,
        );

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].body, "actual summary");
    }

    #[test]
    fn hide_merges_thinking_blocks_separated_only_by_shell_and_web_search() {
        let blocks = vec![
            Block::new(BlockKind::Reasoning, THINKING_TITLE, "first thought"),
            Block::new(BlockKind::Tool, "Running 1 shell command", ""),
            Block::new(BlockKind::Tool, "Web search · rust ownership", ""),
            Block::new(BlockKind::Reasoning, THINKING_TITLE, "latest thought"),
        ];

        let visible =
            visible_transcript_blocks(&blocks, ShellDisplayMode::Hide, DiffDisplayMode::Collapse);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].body, "latest thought");
    }

    #[test]
    fn hide_drops_stale_thinking_across_context_compaction() {
        let blocks = vec![
            Block::new(BlockKind::Reasoning, THINKING_TITLE, "before"),
            Block::new(BlockKind::System, "Context compacted", ""),
            Block::new(BlockKind::Reasoning, THINKING_TITLE, "after"),
        ];

        let visible =
            visible_transcript_blocks(&blocks, ShellDisplayMode::Hide, DiffDisplayMode::Collapse);

        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].title, "Context compacted");
        assert_eq!(visible[1].body, "after");
    }

    #[test]
    fn hide_merges_thinking_across_inline_history_and_new_output() {
        let history = vec![
            Block::new(BlockKind::Reasoning, THINKING_TITLE, "first thought"),
            Block::new(BlockKind::Tool, "Running 1 shell command", ""),
        ];
        let committed = vec![Block::new(
            BlockKind::Reasoning,
            THINKING_TITLE,
            "latest thought",
        )];

        assert!(hidden_thinking_merge_at_history_boundary(
            &history,
            &committed,
            ShellDisplayMode::Hide,
            DiffDisplayMode::Collapse
        ));
    }

    #[test]
    fn hide_relayouts_when_compaction_separates_duplicate_thinking() {
        let history = vec![
            Block::new(BlockKind::Reasoning, THINKING_TITLE, "stale"),
            Block::new(BlockKind::Tool, "Shell · command · completed", ""),
            Block::new(BlockKind::System, "Context compacted", ""),
        ];
        let committed = vec![Block::new(BlockKind::Reasoning, THINKING_TITLE, "latest")];

        assert!(hidden_thinking_merge_at_history_boundary(
            &history,
            &committed,
            ShellDisplayMode::Hide,
            DiffDisplayMode::Collapse
        ));
    }

    #[test]
    fn expanded_shell_group_caps_output_at_five_painted_rows_across_children() {
        let group = Block::shell_group(
            BlockKind::Tool,
            "Shell · 2 commands · all passed",
            vec![
                Block::new(BlockKind::Tool, "Shell · first · exit 0", "one\ntwo\nthree"),
                Block::new(
                    BlockKind::Tool,
                    "Shell · second · exit 0",
                    "four\nfive\nsix",
                ),
            ],
        );

        let lines = shell_group_lines(&group, 80, crate::state::ShellDisplayMode::Expand, false);

        assert_eq!(lines.iter().filter(|line| line.prefix == "    ").count(), 5);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.prefix == "    ")
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three", "four", "five"]
        );
    }

    #[test]
    fn collapsed_shell_heading_ellipsizes_instead_of_wrapping() {
        let block = Block::shell_group(
            BlockKind::Tool,
            "Shell · 123 commands · completed · 123.4s",
            vec![Block::new(BlockKind::Tool, "Shell · detail", "")],
        );

        let lines = block_lines(&block, 20);

        assert_eq!(lines.len(), 1);
        assert!(painted_line_width(&lines[0]) <= 20);
        assert!(lines[0].text.ends_with('…'));
        assert_eq!(lines[0].tool_heading, Some(block.id()));
    }

    #[test]
    fn expanded_shell_group_shows_ordered_children_without_nested_click_targets() {
        let group = Block::shell_group(
            BlockKind::Tool,
            "Shell · 2 commands · all passed",
            vec![
                Block::new(BlockKind::Tool, "Shell · first · exit 0", "one"),
                Block::new(BlockKind::Tool, "Shell · second · exit 0", "two"),
            ],
        );

        let lines = block_lines_with_expansion(&group, 80, true);
        let texts = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            texts,
            [
                "Shell · 2 commands · all passed",
                "Shell · first · exit 0",
                "one",
                "Shell · second · exit 0",
                "two",
            ]
        );
        assert_eq!(lines[0].tool_heading, Some(group.id()));
        assert!(lines[1..].iter().all(|line| line.tool_heading.is_none()));
    }

    #[test]
    fn failed_shell_group_uses_warning_tone() {
        let group = Block::shell_group(
            BlockKind::Warning,
            "Shell · 2 commands · 1 failed",
            vec![
                Block::new(BlockKind::Tool, "Shell · first · exit 0", ""),
                Block::new(BlockKind::Warning, "Shell · second · exit 1", ""),
            ],
        );

        let lines = block_lines(&group, 80);

        assert_eq!(lines[0].tone, Tone::Warning);
    }

    #[test]
    fn long_bash_heading_stays_one_clickable_row() {
        let block = Block::new(BlockKind::Tool, format!("Shell · {}", "x".repeat(100)), "");
        let lines = block_lines(&block, 20);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].tool_heading, Some(block.id()));
        assert!(lines[0].text.ends_with('…'));
    }

    #[test]
    fn clicking_a_bash_heading_toggles_only_that_block() {
        let first = Block::new(BlockKind::Tool, "Shell · first", "one");
        let second = Block::new(BlockKind::Tool, "Shell · second", "two");
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.previous_lines = vec![
            block_lines(&first, 80).remove(0),
            PaintLine::blank(),
            block_lines(&second, 80).remove(0),
        ];

        assert!(renderer.toggle_tool_at(0));
        assert!(renderer.expanded_tools.contains(&first.id()));
        assert!(!renderer.expanded_tools.contains(&second.id()));
        assert!(!renderer.toggle_tool_at(1));
    }

    #[test]
    fn bash_hover_tracks_only_the_heading_text_cells() {
        let block = Block::new(BlockKind::Tool, "Shell · cargo test", "");
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.previous_lines = block_lines(&block, 80);

        assert!(!renderer.hover_at(0, 0));
        assert!(renderer.hover_at(2, 0));
        assert_eq!(renderer.hovered_tool, Some(block.id()));
        assert!(!renderer.hover_at(3, 0));
        assert!(renderer.hover_at(79, 0));
        assert_eq!(renderer.hovered_tool, None);
    }

    #[test]
    fn inline_renderer_ignores_tool_heading_clicks() {
        let block = Block::new(BlockKind::Tool, "Shell · first", "one");
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Inline);
        renderer.previous_lines = block_lines(&block, 80);

        assert!(!renderer.toggle_tool_at(0));
        assert!(renderer.expanded_tools.is_empty());
    }

    #[test]
    fn fullscreen_selection_copies_the_current_screen_and_preserves_clicks() {
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = vec![PaintLine::plain("abcdef")];

        assert!(renderer.begin_selection(2, 0));
        assert!(renderer.update_selection(4, 0));
        assert_eq!(
            renderer.finish_selection(4, 0),
            SelectionResult::Copy("cde".to_owned())
        );

        assert!(renderer.begin_selection(3, 0));
        assert_eq!(
            renderer.finish_selection(3, 0),
            SelectionResult::Click(3, 0)
        );
    }

    #[test]
    fn a_one_character_drag_is_left_unpainted() {
        let lines = vec![PaintLine::plain("a한b"), PaintLine::plain("cd")];
        let range = |start: (u16, usize), end: (u16, usize)| CellRange {
            start: CellPosition {
                column: start.0,
                row: start.1,
            },
            end: CellPosition {
                column: end.0,
                row: end.1,
            },
        };

        // One cell, and both cells of the same wide glyph: still one character.
        assert!(!selection_is_worth_painting(range((0, 0), (0, 0)), &lines));
        assert!(!selection_is_worth_painting(range((1, 0), (2, 0)), &lines));
        // A second character brings the block in, whichever side it comes from.
        assert!(selection_is_worth_painting(range((0, 0), (1, 0)), &lines));
        assert!(selection_is_worth_painting(range((1, 0), (3, 0)), &lines));
        // Across rows the characters still add up rather than counting rows.
        assert!(selection_is_worth_painting(range((2, 0), (0, 1)), &lines));
        assert!(!selection_is_worth_painting(
            range((0, 0), (0, 1)),
            &[PaintLine::blank(), PaintLine::plain("c")]
        ));
    }

    #[test]
    fn fullscreen_selection_is_cancelled_when_the_transcript_scrolls() {
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = vec![PaintLine::plain("abcdef")];
        renderer.wrapped = text_rows(10, "line");

        assert!(renderer.begin_selection(1, 0));
        assert!(renderer.update_selection(3, 0));
        assert!(renderer.scroll(1));
        assert_eq!(renderer.finish_selection(3, 0), SelectionResult::None);
    }

    #[test]
    fn fullscreen_transcript_drag_continues_across_wheel_scrolling() {
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.wrapped = text_rows(10, "line");
        renderer.scroll_back = 2;
        renderer.last_transcript_rows = 4;
        renderer.last_transcript_start = 4;
        renderer.last_transcript_screen_start = 0;
        renderer.previous_lines = renderer.wrapped[4..8].to_vec();

        assert!(renderer.begin_selection(4, 2));
        assert!(renderer.update_selection(0, 0));
        assert!(renderer.scroll(3));

        // The next frame shows older history, but the original anchor remains
        // attached to line6 instead of becoming the new row-two text.
        renderer.last_transcript_start = 1;
        renderer.previous_lines = renderer.wrapped[1..5].to_vec();
        assert!(renderer.update_selection(0, 0));
        assert_eq!(
            renderer.finish_selection(0, 0),
            SelectionResult::Copy(
                ["line1", "line2", "line3", "line4", "line5", "line6"].join("\n")
            )
        );
    }

    #[test]
    fn inline_renderer_never_owns_text_selection() {
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Inline);
        renderer.previous_lines = vec![PaintLine::plain("abcdef")];

        assert!(!renderer.begin_selection(1, 0));
        assert!(!renderer.update_selection(3, 0));
        assert_eq!(renderer.finish_selection(3, 0), SelectionResult::None);
    }

    #[test]
    fn fullscreen_selection_survives_only_while_selected_rows_are_stable() {
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = vec![PaintLine::plain("selected"), PaintLine::plain("status")];

        assert!(renderer.begin_selection(0, 0));
        assert!(renderer.update_selection(2, 0));
        renderer.reconcile_selection(
            &[
                PaintLine::plain("selected"),
                PaintLine::plain("changed status"),
            ],
            0,
        );
        assert_eq!(
            renderer.finish_selection(2, 0),
            SelectionResult::Copy("sel".to_owned())
        );

        assert!(renderer.begin_selection(0, 0));
        assert!(renderer.update_selection(2, 0));
        renderer.reconcile_selection(
            &[PaintLine::plain("replaced"), PaintLine::plain("status")],
            0,
        );
        assert_eq!(renderer.finish_selection(2, 0), SelectionResult::None);
    }

    #[test]
    fn progress_spinner_repaint_keeps_the_drag_selection() {
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        let mut previous = PaintLine::plain("진행 단계");
        previous.prefix = "  ⠋  ".to_owned();
        previous.prefix_tone = Tone::Accent;
        previous.tone = Tone::Accent;
        previous.bold = true;
        let mut spinner = previous.clone();
        spinner.prefix = "  ⠙  ".to_owned();
        renderer.previous_lines = vec![previous];

        assert!(renderer.begin_selection(5, 0));
        assert!(renderer.update_selection(7, 0));
        renderer.reconcile_selection(&[spinner], 1);

        assert!(renderer.selection.range().is_some());
    }

    #[test]
    fn plan_animation_holds_only_the_dragged_rows() {
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = vec![PaintLine::plain("selected"), PaintLine::plain("animated")];
        assert!(renderer.begin_selection(0, 0));
        assert!(renderer.update_selection(2, 0));

        assert!(renderer.animation_row_is_selected(0));
        assert!(!renderer.animation_row_is_selected(1));
        let mut updates = vec![
            (0, PaintLine::plain("held")),
            (1, PaintLine::plain("moving")),
        ];
        updates.retain(|(row, _)| !renderer.animation_row_is_selected(*row));
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, 1);
        assert_eq!(updates[0].1.text, "moving");
        assert_eq!(
            renderer.finish_selection(2, 0),
            SelectionResult::Copy("sel".to_owned())
        );
    }

    #[test]
    fn rewrapping_preserves_expanded_bash_output() {
        let block = Block::new(BlockKind::Tool, "Shell · first", "one\ntwo");
        let id = block.id();
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.history.push(block);
        renderer.expanded_tools.insert(id);

        renderer.rewrap(80);
        assert!(renderer.wrapped.iter().any(|line| line.text == "one"));
        renderer.rewrap(20);

        assert!(renderer.expanded_tools.contains(&id));
        assert!(renderer.wrapped.iter().any(|line| line.text == "two"));
    }

    #[test]
    fn fullscreen_replaces_an_anchored_shell_instead_of_appending_it() {
        let anchor = Block::new(BlockKind::Tool, "Running 1 shell command", "");
        let mut completed = Block::new(BlockKind::Tool, "Shell · 1 command · completed", "done");
        completed.adopt_id(&anchor);
        let mut history = vec![
            Block::new(BlockKind::Assistant, "Before", ""),
            anchor,
            Block::new(BlockKind::Assistant, "After", ""),
        ];

        assert!(replace_history_block(&mut history, completed));
        assert_eq!(history.len(), 3);
        assert_eq!(history[1].title, "Shell · 1 command · completed");
        assert_eq!(history[2].title, "After");
    }

    #[test]
    fn inline_shell_completion_deduplicates_history_before_mode_relayout() {
        let anchor = Block::new(BlockKind::Tool, "Running 1 shell command", "");
        let mut completed = Block::new(BlockKind::Tool, "Shell · 1 command · completed", "done");
        completed.adopt_id(&anchor);
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Inline);

        assert!(replaces_inline_history(
            std::slice::from_ref(&anchor),
            std::slice::from_ref(&completed)
        ));
        assert!(!replaces_inline_history(
            std::slice::from_ref(&anchor),
            &[Block::new(BlockKind::Assistant, "Codex", "new")]
        ));
        renderer.record_inline_history(&[anchor]);
        renderer.record_inline_history(&[completed]);

        assert_eq!(renderer.history.len(), 1);
        for mode in [ShellDisplayMode::Hide, ShellDisplayMode::Collapse] {
            renderer.shell_display_mode = mode;
            let reprinted = renderer
                .history
                .iter()
                .flat_map(|block| {
                    block_group_lines(
                        block,
                        80,
                        renderer.shell_display_mode,
                        renderer.diff_display_mode,
                        renderer.expanded_tools.contains(&block.id()),
                    )
                })
                .map(|line| painted(&line))
                .collect::<Vec<_>>();
            assert!(reprinted.iter().all(|line| !line.contains("Running")));
            match mode {
                ShellDisplayMode::Hide => assert!(reprinted.is_empty()),
                ShellDisplayMode::Collapse => {
                    assert_eq!(
                        reprinted
                            .iter()
                            .filter(|line| !line.is_empty())
                            .collect::<Vec<_>>(),
                        ["▸ Shell · 1 command · completed"]
                    );
                }
                ShellDisplayMode::Expand => unreachable!("not exercised"),
            }
        }
    }

    fn expanding_shell_replacement() -> (Block, Block) {
        let anchor = Block::new(BlockKind::Tool, "Running 1 shell command", "");
        let child = Block::new(
            BlockKind::Tool,
            "Shell · cargo test · exit 0",
            "output one\noutput two\noutput three",
        );
        let mut completed = Block::shell_group(
            BlockKind::Tool,
            "Shell · 1 command · completed",
            vec![child],
        );
        completed.adopt_id(&anchor);
        (anchor, completed)
    }

    fn transcript_view(renderer: &Renderer, view_rows: usize) -> Vec<String> {
        let start = renderer
            .wrapped
            .len()
            .saturating_sub(view_rows)
            .saturating_sub(renderer.scroll_back);
        renderer.wrapped[start..start + view_rows]
            .iter()
            .map(painted)
            .collect()
    }

    fn transcript_rows(count: usize, prefix: &str) -> Vec<Block> {
        (0..count)
            .map(|index| Block::new(BlockKind::Assistant, "Codex", format!("{prefix}{index}")))
            .collect()
    }

    #[test]
    fn fullscreen_replacement_before_the_viewport_keeps_the_same_content_visible() {
        let (anchor, completed) = expanding_shell_replacement();
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.shell_display_mode = ShellDisplayMode::Expand;
        renderer.history = std::iter::once(anchor)
            .chain(transcript_rows(12, "after"))
            .collect();
        renderer.rewrap(80);
        let view_rows = 4;
        let desired_start = 4;
        renderer.scroll_back = renderer.wrapped.len() - view_rows - desired_start;
        let before = transcript_view(&renderer, view_rows);
        let scroll_back = renderer.scroll_back;

        renderer.commit_fullscreen_blocks(&[completed], 80, view_rows);

        assert_eq!(renderer.scroll_back, scroll_back);
        assert_eq!(transcript_view(&renderer, view_rows), before);
    }

    #[test]
    fn fullscreen_replacement_after_the_viewport_adjusts_to_keep_its_content_visible() {
        let (anchor, completed) = expanding_shell_replacement();
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.shell_display_mode = ShellDisplayMode::Expand;
        renderer.history = transcript_rows(12, "before")
            .into_iter()
            .chain(std::iter::once(anchor))
            .collect();
        renderer.rewrap(80);
        let view_rows = 4;
        renderer.scroll_back = renderer.wrapped.len() - view_rows;
        let before = transcript_view(&renderer, view_rows);
        let scroll_back = renderer.scroll_back;

        renderer.commit_fullscreen_blocks(&[completed], 80, view_rows);

        assert!(renderer.scroll_back > scroll_back);
        assert_eq!(transcript_view(&renderer, view_rows), before);
    }

    #[test]
    fn fullscreen_replacement_overlapping_the_viewport_keeps_downstream_content_stable() {
        let (anchor, completed) = expanding_shell_replacement();
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.shell_display_mode = ShellDisplayMode::Expand;
        renderer.history = transcript_rows(4, "before")
            .into_iter()
            .chain(std::iter::once(anchor))
            .chain(transcript_rows(4, "after"))
            .collect();
        renderer.rewrap(80);
        let view_rows = 4;
        let anchor_start = renderer
            .history
            .iter()
            .take(4)
            .flat_map(|block| {
                block_group_lines(
                    block,
                    80,
                    renderer.shell_display_mode,
                    renderer.diff_display_mode,
                    renderer.expanded_tools.contains(&block.id()),
                )
            })
            .count();
        renderer.scroll_back = renderer.wrapped.len() - view_rows - anchor_start;
        let downstream = transcript_view(&renderer, view_rows)
            .last()
            .cloned()
            .expect("visible downstream row");
        let scroll_back = renderer.scroll_back;

        renderer.commit_fullscreen_blocks(&[completed], 80, view_rows);

        assert_eq!(renderer.scroll_back, scroll_back);
        assert_eq!(
            transcript_view(&renderer, view_rows).last(),
            Some(&downstream)
        );
    }

    #[test]
    fn fullscreen_rewrap_adjusts_a_scrolled_reader_by_the_total_row_delta() {
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.history = (0..12)
            .map(|index| {
                Block::new(
                    BlockKind::Assistant,
                    "Codex",
                    format!("long transcript row {index} that wraps when narrow"),
                )
            })
            .collect();
        renderer.rewrap(80);
        renderer.scroll_back = 4;
        let before_rows = renderer.wrapped.len();
        let before_scroll = renderer.scroll_back;

        renderer.commit_fullscreen_blocks(&[], 20, 4);

        let row_delta = renderer.wrapped.len() as isize - before_rows as isize;
        assert!(row_delta > 0);
        assert_eq!(
            renderer.scroll_back,
            before_scroll.saturating_add_signed(row_delta)
        );
    }

    #[test]
    fn thinking_blocks_fold_into_one_italic_paragraph() {
        let block = Block::new(
            BlockKind::Reasoning,
            "Thinking…",
            "**Weighing options**\n\nFirst thought.\n\nSecond thought.",
        );

        let lines = block_lines(&block, 200);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].prefix, "∴ ");
        assert_eq!(
            lines[0].text,
            "**Weighing options** First thought. Second thought."
        );
        assert_eq!(lines[0].tone, Tone::Thinking);
        assert!(!lines[0].bold);
        assert!(lines.iter().all(|line| line.text != "Thinking…"));
    }

    #[test]
    fn empty_thinking_block_shows_the_label() {
        let lines = block_lines(&Block::new(BlockKind::Reasoning, "Thinking…", ""), 80);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].prefix, "✻ ");
        assert_eq!(lines[0].text, "Thinking…");
    }

    #[test]
    fn plan_blocks_keep_their_heading() {
        let lines = block_lines(&Block::new(BlockKind::Reasoning, "Plan", "step one"), 80);

        assert_eq!(lines[0].text, "Plan");
        assert!(lines[0].bold);
        assert_eq!(lines[1].prefix, "  ");
        assert_eq!(lines[1].text, "step one");
    }

    #[test]
    fn plan_blocks_use_the_codex_checkbox_layout() {
        let lines = block_lines(
            &Block::new(
                BlockKind::Plan,
                "작업 단계",
                "└ why\n✔ first\n▸ second\n□ third",
            ),
            80,
        );

        assert_eq!(lines[0].prefix, "- ");
        assert_eq!(lines[0].text, UPDATED_PLAN_TITLE);
        assert!(lines[0].bold);
        assert_eq!(lines[1].prefix, "  └ ");
        assert_eq!(lines[1].text, "why");
        assert_eq!(lines[2].prefix, "    ✔ ");
        assert_eq!(lines[2].text, "first");
        // Done steps are struck through, and the ✔ in the gutter is not.
        assert_eq!(lines[2].tone, Tone::PlanDone);
        assert_eq!(lines[2].prefix_tone, Tone::Muted);
        // The step being worked on is the one row that is lit, not dimmed.
        assert_eq!(lines[3].prefix, "    □ ");
        assert_eq!(lines[3].text, "second");
        assert_eq!(lines[3].tone, Tone::Accent);
        assert!(lines[3].bold);
        assert_eq!(lines[4].prefix, "    □ ");
        assert_eq!(lines[4].text, "third");
        assert_eq!(lines[4].tone, Tone::Muted);
    }

    #[test]
    fn plan_blocks_without_steps_say_so() {
        let lines = block_lines(&Block::new(BlockKind::Plan, "작업 단계", "└ why"), 80);

        assert_eq!(lines[1].text, "why");
        assert_eq!(lines[2].text, "(no steps provided)");
    }

    #[test]
    fn status_line_is_trimmed_to_terminal_width() {
        let line = status_line_row(
            Some(StatusLineView {
                model: Some("GPT-5.6 Codex".to_owned()),
                effort: Some("xhigh".to_owned()),
                context: Some("ctx: 45k/256k (18%)".to_owned()),
                five_hour_percent: Some(12),
                five_hour_remaining: None,
                weekly_percent: Some(34),
                notice: Some("connected".to_owned()),
            }),
            "",
            32,
        );
        assert!(painted_width(&line) <= 32);
        assert!(line.text.trim_start().starts_with("GPT-5.6 Codex"));
        assert!(painted(&line).ends_with("..."));
    }

    #[test]
    fn status_line_keeps_model_and_effort_when_branch_is_removed() {
        let line = status_line_row(
            Some(StatusLineView {
                model: Some("GPT-5.6 Sol".to_owned()),
                effort: Some("high".to_owned()),
                context: None,
                five_hour_percent: None,
                five_hour_remaining: None,
                weekly_percent: None,
                notice: None,
            }),
            "",
            30,
        );

        assert!(painted(&line).contains("GPT-5.6 Sol"));
        assert!(painted(&line).contains("◆ high"));
    }

    #[test]
    fn status_line_places_the_model_and_effort_shortcuts_at_the_far_right() {
        let line = status_line_row(
            Some(StatusLineView {
                model: Some("GPT-5.6 Codex".to_owned()),
                effort: Some("xhigh".to_owned()),
                context: Some("ctx: 45k/256k (18%)".to_owned()),
                five_hour_percent: Some(12),
                five_hour_remaining: None,
                weekly_percent: Some(34),
                notice: None,
            }),
            "",
            120,
        );

        assert!(painted(&line).ends_with("Shift + ↑↓ model · ←→ effort"));
        assert_eq!(painted_width(&line), 118);
    }

    #[test]
    fn opening_the_side_panel_hides_only_composer_context() {
        let mut status = Some(StatusLineView {
            model: Some("GPT-5.6 Codex".to_owned()),
            effort: Some("xhigh".to_owned()),
            context: Some("ctx: 164k/258k (63%)".to_owned()),
            five_hour_percent: Some(14),
            five_hour_remaining: Some("3h 6m".to_owned()),
            weekly_percent: Some(27),
            notice: None,
        });

        let footer = move_context_to_side_panel(&mut status, 44);
        let line = status_line_row(status, "", 120);

        assert!(painted(&line).contains("GPT-5.6 Codex"));
        assert!(painted(&line).contains("◆ xhigh"));
        assert!(!painted(&line).contains("ctx:"));
        assert!(painted(&line).contains("3h 6m: 14%"));
        assert!(painted(&line).contains("week: 27%"));
        assert!(painted(&line).ends_with("Shift + ↑↓ model · ←→ effort"));
        assert_eq!(footer.len(), 2);
        assert_eq!(painted(&footer[0]), "─".repeat(44));
        assert_eq!(footer[0].tone, Tone::SidePanelDivider);
        assert!(painted(&footer[1]).starts_with("Context: "));
        assert!(painted(&footer[1]).ends_with("164/258K (63%)"));
        assert_eq!(footer[1].tail[0].tone, Tone::Model56);
    }

    #[test]
    fn status_line_places_the_five_hour_countdown_before_its_percent() {
        let line = status_line_row(
            Some(StatusLineView {
                model: None,
                effort: None,
                context: None,
                five_hour_percent: Some(3),
                five_hour_remaining: Some("4h 38m".to_owned()),
                weekly_percent: None,
                notice: None,
            }),
            "",
            80,
        );

        assert!(painted(&line).contains("4h 38m: 3%"));
    }

    #[test]
    fn status_line_omits_a_disabled_branch_slot() {
        let line = status_line_row(
            Some(StatusLineView {
                model: Some("GPT-5.6 Sol".to_owned()),
                effort: Some("high".to_owned()),
                context: None,
                five_hour_percent: None,
                five_hour_remaining: None,
                weekly_percent: None,
                notice: None,
            }),
            "",
            80,
        );

        assert_eq!(line.prefix, " ");
        assert_eq!(line.text, " GPT-5.6 Sol ");
    }

    /// The two readings the status line lets you change answer to a click; the
    /// ones that only report — context and limits — do not.
    #[test]
    fn the_model_and_effort_readings_are_the_only_clickable_status_spans() {
        let line = status_line_row(
            Some(StatusLineView {
                model: Some("GPT-5.6 Sol".to_owned()),
                effort: Some("high".to_owned()),
                context: Some("ctx: 45k/256k (18%)".to_owned()),
                five_hour_percent: Some(12),
                five_hour_remaining: None,
                weekly_percent: Some(34),
                notice: None,
            }),
            "",
            80,
        );

        assert_eq!(pick_on(&line, "GPT-5.6 Sol"), Some(Pick::Model));
        assert_eq!(pick_on(&line, "high"), Some(Pick::EffortSetting));
        assert_eq!(pick_on(&line, "main"), None);
        assert_eq!(pick_on(&line, "45k/256k"), None);
        assert_eq!(pick_on(&line, "5h: 12%"), None);
        assert_eq!(pick_on(&line, "week: 34%"), None);
        assert_eq!(line.tone, Tone::StatusModelSol);
        assert!(word_background(Tone::StatusModelSol).is_none());
        assert!(word_background(Tone::StatusEffortHigh).is_none());
        assert_eq!(
            line.tail
                .iter()
                .find(|span| span.text == "◆ high")
                .map(|span| span.tone),
            Some(Tone::StatusEffortHigh)
        );
    }

    #[test]
    fn effort_status_reading_has_no_unicode_width_override() {
        let line = status_line_row(
            Some(StatusLineView {
                model: None,
                effort: Some("high".to_owned()),
                context: None,
                five_hour_percent: None,
                five_hour_remaining: None,
                weekly_percent: None,
                notice: None,
            }),
            "",
            80,
        );

        assert!(painted(&line).contains("◆ high"));
    }

    /// Before the first status arrives the row is a plain fallback string, with no
    /// model or effort on it to click.
    #[test]
    fn the_status_fallback_row_has_nothing_to_click() {
        assert!(status_line_row(None, "starting…", 40).pick.is_none());
    }

    #[test]
    fn a_hidden_status_area_does_not_paint_a_status_line_row() {
        let editor = Editor::default();
        let frame = normal_frame(
            &[],
            &editor,
            None,
            &[],
            None,
            StatusArea {
                fallback: HIDDEN_STATUS_LINE.to_owned(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        assert!(painted(frame.lines.last().expect("composer bottom rule")).starts_with('╰'));
    }

    /// The compaction row spends its spare columns on a bar, and gives them back
    /// to elapsed time when the terminal has none to spare.
    #[test]
    fn the_compacting_row_moves_one_block_left_to_right() {
        let entering = activity_lines("Compacting.. (4s)", None, 0.25, 80);
        let passing = activity_lines("Compacting.. (4s)", None, 0.5, 80);
        let leaving = activity_lines("Compacting.. (4s)", None, 0.9, 80);

        assert_eq!(
            painted(&entering[0]),
            " ⠹ Compacting.. ░░████░░░░░░░░░░░░░░ (4s)"
        );
        assert_eq!(
            painted(&passing[0]),
            " ⠴ Compacting.. ░░░░░░░░████░░░░░░░░ (4s)"
        );
        assert_eq!(
            painted(&leaving[0]),
            " ⠏ Compacting.. ░░░░░░░░░░░░░░░░░░██ (4s)"
        );

        let narrow = activity_lines("Compacting.. (4s)", None, 0.5, 30);
        let narrow = painted(&narrow[0]);

        assert_eq!(narrow, " ⠴ Compacting.. (4s)");
    }

    #[test]
    fn the_compacting_label_shimmers_independently_from_its_progress_block() {
        let line = activity_lines_with_progress("Compacting.. (4s)", None, 0.0, 0.5, 80);

        assert_eq!(
            painted(&line[0]),
            " ⠋ Compacting.. ░░░░░░░░████░░░░░░░░ (4s)"
        );
    }

    /// The reading belongs to compaction alone: an ordinary turn keeps its plain
    /// elapsed tail even though both rows share the loader.
    #[test]
    fn a_working_row_carries_no_progress_bar() {
        let line = activity_lines("Working.. (4s)", None, 0.0, 80);

        assert_eq!(painted(&line[0]), " ⠋ Working.. (4s)");
    }

    fn painted(line: &PaintLine) -> String {
        let mut out = line.prefix.clone();
        out.push_str(&line.text);
        for span in &line.tail {
            out.push_str(&span.text);
        }
        out
    }

    fn painted_width(line: &PaintLine) -> usize {
        UnicodeWidthStr::width(line.prefix.as_str())
            + UnicodeWidthStr::width(line.text.as_str())
            + line
                .tail
                .iter()
                .map(|span| UnicodeWidthStr::width(span.text.as_str()))
                .sum::<usize>()
    }

    fn test_welcome() -> WelcomeView {
        WelcomeView {
            provider: "Codex".to_owned(),
            plan: "Pro Lite".to_owned(),
            credits: vec!["3 available".to_owned(), "· 2026-08-01  6d left".to_owned()],
            cwd: "C:/Source/DevezVibe".to_owned(),
            account: "dev@example.com".to_owned(),
        }
    }

    #[test]
    fn the_welcome_card_is_two_borderless_rows_under_one_blank_row() {
        for width in [28u16, 70, 140] {
            let lines = welcome_lines(test_welcome(), width);

            assert_eq!(
                lines.len(),
                3,
                "width {width}: expected one blank row and two content rows"
            );
            assert!(
                lines[0] == PaintLine::blank(),
                "width {width}: the leading blank row is missing"
            );
            assert_eq!(
                painted(&lines[1]),
                format!("DEVEZ VIBE  v{}", crate::update::CURRENT_VERSION),
                "width {width}: headline changed"
            );
            assert!(
                painted(&lines[2]).contains("DevezVibe"),
                "width {width}: folder missing — {}",
                painted(&lines[2])
            );
            assert!(
                lines
                    .iter()
                    .all(|line| !painted(line).contains(['╭', '│', '╰'])),
                "width {width}: card still draws a border"
            );
            assert!(
                lines
                    .iter()
                    .all(|line| painted_width(line) <= width as usize),
                "width {width}: a row overflows the terminal"
            );
        }
    }

    #[test]
    fn the_welcome_card_drops_plan_credits_and_release_notes() {
        let painted_card = welcome_lines(test_welcome(), 110)
            .iter()
            .map(painted)
            .collect::<Vec<_>>()
            .join("\n");

        for gone in ["Plan", "Resets", "Account", "What's new", "2026-08-01"] {
            assert!(
                !painted_card.contains(gone),
                "{gone} still on the welcome card: {painted_card}"
            );
        }
    }

    #[test]
    fn commands_panel_closes_its_top_right_corner_at_every_width() {
        let suggestions = vec![
            SuggestionView {
                command: "/model".to_owned(),
                description: "Switch model and reasoning".to_owned(),
                selected: true,
                category: None,
                panel_title: "Commands",
                hint: None,
            },
            SuggestionView {
                command: "/effort".to_owned(),
                description: "Set reasoning effort".to_owned(),
                selected: false,
                category: None,
                panel_title: "Commands",
                hint: None,
            },
        ];

        for width in [40u16, 80, 160] {
            let lines = suggestion_lines(&suggestions, width);
            let top = painted(&lines[0]);

            assert!(top.starts_with("╭─ Commands "), "width {width}: {top}");
            assert!(
                top.ends_with('╮'),
                "width {width}: top-right corner missing"
            );
            assert!(
                lines
                    .iter()
                    .all(|line| painted_width(line) == panel_span(width)),
                "width {width}: rows are not all {} columns",
                panel_span(width)
            );
        }
    }

    #[test]
    fn suggestion_options_stay_on_one_physical_row() {
        let suggestions = vec![SuggestionView {
            command: "/renderer".to_owned(),
            description: "Pin the composer\n…or use terminal scrollback".to_owned(),
            selected: true,
            category: None,
            panel_title: "Commands",
            hint: None,
        }];

        let lines = suggestion_lines(&suggestions, 40);
        let option = lines
            .iter()
            .find(|line| line.text.contains("/renderer") || line.text.contains("scrollback"))
            .expect("option row");

        assert!(!painted(option).contains(['\r', '\n']));
        assert_eq!(painted_width(option), panel_span(40));
    }

    #[test]
    fn command_dock_keeps_the_command_name_when_the_description_overflows() {
        let suggestions = vec![SuggestionView {
            command: "/renderer".to_owned(),
            description: "Pin the composer (fullscreen) or use terminal scrollback (inline)"
                .to_owned(),
            selected: false,
            category: None,
            panel_title: "Commands",
            hint: None,
        }];

        let lines = suggestion_lines(&suggestions, 72);
        let option = &lines[2];

        assert!(painted(option).contains("/renderer"));
        assert!(!painted(option).contains("…nderer"));
        assert_eq!(painted_width(option), panel_span(72));
    }

    #[test]
    fn command_dock_aligns_descriptions_after_long_command_names() {
        let suggestions = vec![
            SuggestionView {
                command: "/model".to_owned(),
                description: "Switch model".to_owned(),
                selected: true,
                category: None,
                panel_title: "Commands",
                hint: None,
            },
            SuggestionView {
                command: "/reload-plugins".to_owned(),
                description: "Apply plugin changes".to_owned(),
                selected: false,
                category: None,
                panel_title: "Commands",
                hint: None,
            },
        ];

        let lines = suggestion_lines(&suggestions, 80);
        let short_name_row = painted(&lines[2]);
        let long_name_row = painted(&lines[3]);
        let short_description = short_name_row
            .find("Switch model")
            .expect("short description");
        let long_description = long_name_row
            .find("Apply plugin changes")
            .expect("long description");

        assert_eq!(
            UnicodeWidthStr::width(&short_name_row[..short_description]),
            UnicodeWidthStr::width(&long_name_row[..long_description])
        );
    }

    #[test]
    fn completion_panel_uses_dynamic_heading_categories_and_hint() {
        let suggestions = vec![
            SuggestionView {
                command: "review".to_owned(),
                description: "Review a change".to_owned(),
                selected: true,
                category: Some("Skill".to_owned()),
                panel_title: "Mentions",
                hint: Some(
                    "←/→ mode  ·  All Results  ·  Enter/Tab insert  ·  Esc close".to_owned(),
                ),
            },
            SuggestionView {
                command: "src/main.rs".to_owned(),
                description: String::new(),
                selected: false,
                category: Some("File".to_owned()),
                panel_title: "Mentions",
                hint: None,
            },
        ];

        let lines = suggestion_lines(&suggestions, 80)
            .iter()
            .map(painted)
            .collect::<Vec<_>>();

        assert!(lines[0].starts_with("╭─ Mentions "));
        assert!(lines.iter().any(|line| line.contains("[Skill] review")));
        assert!(lines.iter().any(|line| line.contains("[File]")));
        assert!(lines.iter().any(|line| line.contains("All Results")));
    }

    #[test]
    fn panel_overlay_keeps_its_border_when_a_row_folds() {
        // An unbreakable run far wider than the terminal, like an OAuth URL.
        let long = "a".repeat(400);
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "Sign in to ChatGPT".to_owned(),
                lines: vec![OverlayLine {
                    text: long,
                    selected: false,
                    muted: true,
                }],
                slider: None,
                hint: "Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        let panel = frame
            .lines
            .iter()
            .filter(|line| {
                let painted = painted(line);
                painted.starts_with('│') || painted.starts_with('╭') || painted.starts_with('╰')
            })
            .collect::<Vec<_>>();
        let body = panel
            .iter()
            .filter(|line| line.prefix.starts_with('│'))
            .collect::<Vec<_>>();

        assert!(body.len() > 1, "the row should have folded");
        // Every folded row keeps the left border instead of blanking it out.
        assert!(
            body.iter().all(|line| line.prefix.starts_with('│')),
            "a folded row lost its border"
        );
        // Closed box: identical width on every row, and corners on the rules.
        let expected = panel_span(80);
        assert!(
            panel.iter().all(|line| painted_width(line) == expected),
            "panel rows are not all {expected} columns: {:?}",
            panel
                .iter()
                .map(|line| painted_width(line))
                .collect::<Vec<_>>()
        );
        assert!(painted(panel[0]).ends_with('╮'), "top-right corner missing");
        assert!(
            painted(panel.last().expect("bottom rule")).ends_with('╯'),
            "bottom-right corner missing"
        );
        assert!(
            body.iter().all(|line| painted(line).ends_with('│')),
            "a body row lost its right border"
        );
    }

    #[test]
    fn panel_option_detail_sits_two_columns_under_its_label() {
        // A question row, a selected option, and an option whose detail folds.
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "증상".to_owned(),
                lines: vec![
                    OverlayLine {
                        text: "Claude resume 시 어떤 증상인가?".to_owned(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: format!("화면에 이전 대화 안 보임\n{}", "상세 ".repeat(30)),
                        selected: true,
                        muted: false,
                    },
                ],
                slider: None,
                hint: "↑↓ 선택  Enter 확인  Esc 취소".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        let body = frame
            .lines
            .iter()
            .filter(|line| line.prefix.starts_with('│') && !line.text.trim().is_empty())
            .collect::<Vec<_>>();
        let indent = |line: &PaintLine| UnicodeWidthStr::width(line.prefix.as_str());

        let label = body
            .iter()
            .find(|line| line.text.starts_with("화면에"))
            .expect("option label");
        let details = body
            .iter()
            .filter(|line| line.text.starts_with("상세"))
            .collect::<Vec<_>>();

        assert!(details.len() > 1, "the detail should have folded");
        // The detail is the label's quieter half: two columns in, dimmed, and
        // every folded row of it stays on that same indent.
        assert!(
            details.iter().all(|line| indent(line) == indent(label) + 2),
            "detail indents {:?} do not sit two columns under the label at {}",
            details.iter().map(|line| indent(line)).collect::<Vec<_>>(),
            indent(label)
        );
        assert!(
            details.iter().all(|line| line.tone == Tone::Muted),
            "a detail row is not dimmed"
        );
        assert!(
            body.iter().all(|line| painted(line).ends_with('│')),
            "a body row lost its right border"
        );
    }

    #[test]
    fn question_overlay_free_text_shows_the_typed_answer() {
        let mut editor = Editor::default();
        editor.insert_str("답변");
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "테스트".to_owned(),
                lines: vec![OverlayLine {
                    text: "테스트 선택지 중 어느 것 고를래?".to_owned(),
                    selected: false,
                    muted: false,
                }],
                slider: None,
                hint: "Enter 전송 · Esc 취소".to_owned(),
                style: OverlayStyle::Question,
                input: Some(&editor),
                input_label: "Answer",
                input_placeholder: "Type your answer…",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        let painted = frame.lines.iter().map(painted).collect::<Vec<_>>();

        assert!(
            painted.iter().any(|line| line.contains("답변")),
            "typed answer missing from {painted:?}"
        );
        assert!(
            !painted.iter().any(|line| line.contains("Type your answer")),
            "placeholder still shown alongside typed text"
        );
    }

    /// End to end: what the keys did has to be what the panel paints, so the
    /// answer is followed from the question arriving to the row it lands on.
    #[test]
    fn a_typed_question_answer_reaches_the_painted_panel() {
        let mut state = crate::state::AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            Vec::new(),
            "gpt-5.6-sol",
            None,
        );
        state.begin_server_request(
            serde_json::json!(1),
            "item/tool/requestUserInput",
            &serde_json::json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": [{ "label": "첫째", "description": "설명" }]
                }]
            }),
        );
        // Row 2 is the free-text row: one option, then 직접 입력.
        for key in ["2", "답", "변"] {
            let ch = key.chars().next().expect("key");
            state.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(ch),
                crossterm::event::KeyModifiers::NONE,
            ));
        }

        let view = state.view();
        let frame = overlay_frame(
            &[],
            view.overlay.expect("the question panel is open"),
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        let painted = frame.lines.iter().map(painted).collect::<Vec<_>>();

        assert!(
            painted.iter().any(|line| line.contains("답변")),
            "the typed answer never reached the panel: {painted:?}"
        );
    }

    /// The answer is written on the row it was picked on, with the options still
    /// around it, rather than in a box that replaced the question.
    #[test]
    fn question_overlay_types_a_free_text_answer_on_its_own_row() {
        let mut editor = Editor::default();
        editor.insert_str("직접 쓴 답");
        fn overlay(editor: Option<&Editor>) -> OverlayView<'_> {
            OverlayView {
                closable: false,
                title: "테스트".to_owned(),
                lines: vec![
                    OverlayLine {
                        text: "어느 것 고를래?".to_owned(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: "선택지 A\n첫 번째".to_owned(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: "직접 입력".to_owned(),
                        selected: true,
                        muted: false,
                    },
                    OverlayLine {
                        text: "이 내용으로 대화하기".to_owned(),
                        selected: false,
                        muted: false,
                    },
                ],
                slider: None,
                hint: "Enter 전송 · Esc 취소".to_owned(),
                style: OverlayStyle::Question,
                input: editor,
                input_label: "Answer",
                input_placeholder: "여기에 직접 입력…",
            }
        }
        let status = || StatusArea {
            fallback: String::new(),
            line: None,
            composer_notice: None,
            composer_mode: None,
        };

        let frame = overlay_frame(&[], overlay(Some(&editor)), None, status(), 80);
        let typed = frame.lines.iter().map(painted).collect::<Vec<_>>();
        let answer_row = typed
            .iter()
            .position(|line| line.contains("직접 쓴 답"))
            .expect("the typed answer is missing");

        assert!(typed[answer_row].starts_with("│ ❯ 2. 직접 쓴 답"));
        assert!(
            typed.iter().any(|line| line.contains("선택지 A")),
            "the options left the screen while the answer was typed"
        );
        assert!(
            !typed.iter().any(|line| line.contains("여기에 직접 입력")),
            "the placeholder stayed next to the typed answer"
        );
        // The cursor belongs at the end of what was typed, on that same row.
        assert!(frame.show_cursor);
        assert_eq!(frame.cursor_line, answer_row);
        assert_eq!(
            frame.cursor_col,
            UnicodeWidthStr::width("│ ❯ 2. 직접 쓴 답")
        );

        // Empty, no label or placeholder occupies the cells where Windows
        // Terminal paints an active IME composition.
        let empty = Editor::default();
        let empty_frame = overlay_frame(&[], overlay(Some(&empty)), None, status(), 80);
        let empty_painted = empty_frame.lines.iter().map(painted).collect::<Vec<_>>();
        assert!(
            empty_painted.iter().any(|line| line.starts_with("│ ❯ 2. ")),
            "the empty answer row lost its selected row: {empty_painted:?}"
        );
        assert!(
            !empty_painted
                .iter()
                .any(|line| line.contains("여기에 직접 입력")),
            "the placeholder still occupies the IME cells: {empty_painted:?}"
        );
    }

    #[test]
    fn question_overlay_numbers_its_options_and_rules_off_the_way_out() {
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "테스트".to_owned(),
                lines: vec![
                    OverlayLine {
                        text: "테스트 선택지 중 어느 것 고를래?".to_owned(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: "선택지 A\n첫 번째 테스트 옵션".to_owned(),
                        selected: true,
                        muted: false,
                    },
                    OverlayLine {
                        text: "선택지 B\n두 번째 테스트 옵션".to_owned(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: "직접 입력".to_owned(),
                        selected: false,
                        muted: true,
                    },
                    OverlayLine {
                        text: "이 내용으로 대화하기".to_owned(),
                        selected: false,
                        muted: false,
                    },
                ],
                slider: None,
                hint: "Enter 선택 · ↑/↓ 이동 · Esc 취소".to_owned(),
                style: OverlayStyle::Question,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        let painted = frame.lines.iter().map(painted).collect::<Vec<_>>();
        let row = |needle: &str| {
            painted
                .iter()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("row {needle} missing"))
                .clone()
        };

        assert!(painted[0].starts_with("╭─ 테스트 "));
        assert!(painted[0].ends_with('╮'));
        assert!(row("선택지 A").starts_with("│ ❯ 1. 선택지 A"));
        assert!(row("선택지 B").starts_with("│   2. 선택지 B"));
        assert!(row("직접 입력").starts_with("│   3. "));
        assert!(row("이 내용으로 대화하기").starts_with("│   4. "));
        assert!(
            painted
                .iter()
                .filter(|line| line.starts_with(['╭', '│', '├', '╰']))
                .all(|line| UnicodeWidthStr::width(line.as_str()) == panel_span(80)),
            "question panel rows must keep the picker width"
        );
        // The selection moves the marker, not the panel's own edge.
        let selected = frame
            .lines
            .iter()
            .find(|line| line.tail.iter().any(|span| span.text.contains("선택지 A")))
            .expect("selected row");
        assert_eq!(selected.prefix, "│");
        assert_eq!(selected.prefix_tone, Tone::Border);
        assert_eq!(selected.tone, Tone::Accent);
        // A detail line starts where its label does, not where the number does.
        let column = |line: &str, needle: &str| {
            UnicodeWidthStr::width(&line[..line.find(needle).expect("needle")])
        };
        assert_eq!(
            column(&row("선택지 A"), "선택지"),
            column(&row("첫 번째"), "첫")
        );
        // The way out is ruled off from the answers above it.
        let rule = painted
            .iter()
            .position(|line| line.starts_with('├'))
            .expect("rule row");
        let chat = painted
            .iter()
            .position(|line| line.contains("이 내용으로 대화하기"))
            .expect("chat row");
        assert_eq!(rule + 1, chat);
    }

    #[test]
    fn compact_panel_keeps_each_option_on_one_physical_row() {
        let live = [Block::new(BlockKind::Assistant, "Codex", "existing reply")];
        let frame = overlay_frame(
            &live,
            OverlayView {
                closable: false,
                title: "Resume session".to_owned(),
                lines: vec![OverlayLine {
                    text: "4s ago    a very long session title\nC:\\work\\other".repeat(8),
                    selected: true,
                    muted: false,
                }],
                slider: None,
                hint: "↑↓ navigate  Enter resume  Esc cancel".to_owned(),
                style: OverlayStyle::CompactPanel,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            40,
        );

        let option_rows = frame
            .lines
            .iter()
            .filter(|line| painted(line).contains("4s ago"))
            .collect::<Vec<_>>();
        assert_eq!(option_rows.len(), 1);
        assert_eq!(painted_width(option_rows[0]), panel_span(40));
        assert!(painted(option_rows[0]).contains('…'));
        let option = painted(option_rows[0]);
        assert!(
            option.ends_with("…   │"),
            "truncated row lost its three-column right inset: {option}"
        );
        assert!(
            frame
                .lines
                .iter()
                .any(|line| painted(line).contains("existing reply"))
        );
    }

    #[test]
    fn a_panel_pads_inside_its_borders() {
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "Resume session".to_owned(),
                lines: vec![OverlayLine {
                    text: "yesterday's session".to_owned(),
                    selected: true,
                    muted: false,
                }],
                slider: None,
                hint: "Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        let body = frame
            .lines
            .iter()
            .filter(|line| line.prefix.starts_with('│'))
            .collect::<Vec<_>>();

        assert_eq!(body.len(), 3, "the row should sit between two padding rows");
        for row in [body[0], body[2]] {
            let painted = painted(row);
            assert!(painted.ends_with('│'), "padding row lost its right border");
            assert!(
                painted.trim_matches(|ch| ch == '│' || ch == ' ').is_empty(),
                "padding row is not blank: {painted}"
            );
        }
        assert!(
            body.iter()
                .all(|line| painted_width(line) == panel_span(80)),
            "padding rows do not match the panel width"
        );
    }

    #[test]
    fn a_picker_with_a_search_field_keeps_a_gap_above_the_composer() {
        let editor = Editor::default();
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "Resume session · 1 · this folder".to_owned(),
                lines: vec![OverlayLine {
                    text: "yesterday's session  ·  2h ago".to_owned(),
                    selected: true,
                    muted: false,
                }],
                slider: None,
                hint: "↑↓ navigate  Enter resume  Esc cancel".to_owned(),
                style: OverlayStyle::CompactPanel,
                input: Some(&editor),
                input_label: "Search",
                input_placeholder: "Search by name…",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        let bottom = frame
            .lines
            .iter()
            .position(|line| painted(line).ends_with('╯'))
            .expect("panel bottom rule");
        assert!(
            painted(&frame.lines[bottom + 1]).trim().is_empty(),
            "the session list runs straight into the composer"
        );
        assert!(
            !painted(&frame.lines[bottom + 2]).trim().is_empty(),
            "the composer should start right after the gap"
        );
        let status = frame.lines.len() - 1;
        assert!(
            painted(&frame.lines[status - 1]).trim().is_empty(),
            "the overlay input runs straight into the statusline"
        );
        assert!(
            !painted(&frame.lines[status - 2]).trim().is_empty(),
            "there must be exactly one blank row before the statusline"
        );
    }

    /// A picker docks over the transcript, so opening `/model`, `/effort` or
    /// `/resume` used to blank the welcome card for as long as it was up.
    #[test]
    fn a_docked_picker_keeps_the_welcome_card_on_screen() {
        let welcome = WelcomeView {
            provider: "Codex".to_owned(),
            plan: "Pro".to_owned(),
            cwd: r"C:\Source\DevezVibe".to_owned(),
            account: "someone@example.com".to_owned(),
            credits: vec!["in 3h".to_owned()],
        };
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "Select model".to_owned(),
                lines: vec![OverlayLine {
                    text: "GPT-5.6-Sol".to_owned(),
                    selected: true,
                    muted: false,
                }],
                slider: None,
                hint: "↑↓ model   Enter select".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            Some(welcome),
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        let painted = frame
            .lines
            .iter()
            .map(painted)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(painted.contains("DEVEZ VIBE"), "{painted}");
        assert!(painted.contains(r"C:\Source\DevezVibe"), "{painted}");
        // The card sits above the picker rather than replacing it, one blank row down.
        assert!(frame.lines[0] == PaintLine::blank());
        assert!(frame.lines[1].text.starts_with("DEVEZ VIBE"));
        assert!(painted.contains("Select model"));
        assert!(frame.dock_index > 0, "the picker docks below the card");
    }

    #[test]
    fn picker_overlay_matches_the_commands_closed_panel() {
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "Model".to_owned(),
                lines: vec![
                    OverlayLine {
                        text: "GPT-5.6-Sol".to_owned(),
                        selected: true,
                        muted: false,
                    },
                    OverlayLine {
                        text: "GPT-5.6-Terra".to_owned(),
                        selected: false,
                        muted: false,
                    },
                ],
                slider: None,
                hint: "↑↓ model  ·  Enter select".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        let panel = &frame.lines[..frame.lines.len() - 2];
        assert!(painted(&panel[0]).starts_with("╭─ Model "));
        assert!(painted(&panel[0]).ends_with('╮'));
        assert!(painted(panel.last().expect("panel bottom")).ends_with('╯'));
        assert!(
            panel
                .iter()
                .all(|line| painted_width(line) == panel_span(80)),
            "every panel row must match the Commands panel width"
        );
        assert!(
            panel
                .iter()
                .filter(|line| painted(line).starts_with('│'))
                .all(|line| painted(line).ends_with('│')),
            "every picker body row must keep both side borders"
        );
        let selected = panel
            .iter()
            .find(|line| line.prefix.contains('❯'))
            .expect("selected model row");
        assert_eq!(selected.tone, Tone::ModelSol);
    }

    #[test]
    fn claude_model_picker_uses_each_model_family_tone() {
        let models = [
            ("1. Opus 5", Tone::ModelOpus),
            ("2. Fable 5", Tone::ModelFable),
            ("3. Sonnet 5", Tone::ModelSonnet),
            ("4. Haiku 4.5", Tone::ModelHaiku),
        ];
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: true,
                title: "Model".to_owned(),
                lines: models
                    .iter()
                    .enumerate()
                    .map(|(index, (text, _))| OverlayLine {
                        text: (*text).to_owned(),
                        selected: index == 0,
                        muted: false,
                    })
                    .collect(),
                slider: None,
                hint: "Enter select".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        for (text, tone) in models {
            let line = frame
                .lines
                .iter()
                .find(|line| painted(line).contains(text))
                .expect("Claude model row");
            assert_eq!(line.tone, tone);
        }
    }

    #[test]
    fn narrow_effort_pickers_keep_every_row_inside_the_closed_panel() {
        for width in 20..=33 {
            let frame = overlay_frame(
                &[],
                OverlayView {
                    closable: false,
                    title: "Effort".to_owned(),
                    lines: Vec::new(),
                    slider: Some(EffortSlider {
                        efforts: ["low", "medium", "high", "xhigh", "max", "ultra"]
                            .map(ToOwned::to_owned)
                            .to_vec(),
                        selected: 2,
                        detail: None,
                    }),
                    hint: "Enter select".to_owned(),
                    style: OverlayStyle::Picker,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                },
                None,
                StatusArea {
                    fallback: "status".to_owned(),
                    line: None,
                    composer_notice: None,
                    composer_mode: None,
                },
                width,
            );
            let panel = &frame.lines[..frame.lines.len() - 2];

            assert!(
                panel
                    .iter()
                    .all(|line| painted_width(line) == panel_span(width)),
                "width {width}: Picker row escapes its closed panel"
            );
            assert!(
                panel.iter().any(|line| painted(line).contains("│ HIGH │")),
                "width {width}: selected effort border disappeared"
            );
        }
    }

    #[test]
    fn every_overlay_keeps_exactly_one_blank_row_before_the_statusline() {
        for style in [
            OverlayStyle::Picker,
            OverlayStyle::Panel,
            OverlayStyle::CompactPanel,
            OverlayStyle::Question,
        ] {
            let frame = overlay_frame(
                &[],
                OverlayView {
                    closable: false,
                    title: "Overlay".to_owned(),
                    lines: vec![OverlayLine {
                        text: "choice".to_owned(),
                        selected: true,
                        muted: false,
                    }],
                    slider: None,
                    hint: "Enter confirm".to_owned(),
                    style,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                },
                None,
                StatusArea {
                    fallback: "status".to_owned(),
                    line: None,
                    composer_notice: None,
                    composer_mode: None,
                },
                80,
            );

            let status = frame.lines.len() - 1;
            assert!(painted(&frame.lines[status - 1]).trim().is_empty());
            assert!(!painted(&frame.lines[status - 2]).trim().is_empty());
        }
    }

    #[test]
    fn effort_steps_replace_the_axis_with_one_coloured_row() {
        let slider = EffortSlider {
            efforts: ["low", "medium", "high", "xhigh", "max", "ultra"]
                .map(ToOwned::to_owned)
                .to_vec(),
            selected: 2,
            detail: None,
        };

        let lines = effort_step_lines(&slider, 100);

        // The track opens straight on its box: the blank above it belongs to
        // whatever placed the track, not to the track itself.
        assert_eq!(lines.len(), 3);
        let top = &lines[0];
        let steps = &lines[1];
        let bottom = &lines[2];
        assert_eq!(
            painted(steps).trim(),
            "low › medium › │ HIGH │ › xhigh › max › ultra"
        );
        assert_eq!(steps.prefix, " ".repeat(27));
        assert_eq!(painted(top).trim(), "╭──────╮");
        assert_eq!(painted(bottom).trim(), "╰──────╯");
        assert_eq!(top.prefix, " ".repeat(42));
        assert_eq!(bottom.prefix, top.prefix);
        assert_eq!(top.tone, Tone::EffortHigh);
        assert_eq!(bottom.tone, Tone::EffortHigh);
        let text = painted(steps);
        assert!(!text.contains("Faster"));
        assert!(!text.contains("Smarter"));
        assert!(!text.contains('▲'));
        assert!(!text.contains('─'));

        let selected = steps
            .tail
            .iter()
            .position(|span| span.text == "│ HIGH │")
            .expect("selected effort");
        assert_eq!(steps.tail[selected].tone, Tone::EffortHigh);
        assert!(steps.tail[selected].bold);
        assert_eq!(steps.tail[selected + 1].text, " › ");
        assert_eq!(steps.tail[selected + 1].tone, Tone::EffortHigh);
    }

    /// Each step of the track answers for its own effort, and the separators for
    /// nothing — the shrinking that drops steps on a narrow panel must not shift
    /// which step a click lands on.
    #[test]
    fn every_effort_step_answers_for_its_own_index() {
        let slider = EffortSlider {
            efforts: ["low", "medium", "high", "xhigh", "max", "ultra"]
                .map(ToOwned::to_owned)
                .to_vec(),
            selected: 2,
            detail: None,
        };

        for width in [100, 40] {
            let steps = effort_step_lines(&slider, width).remove(1);
            let labels = steps
                .tail
                .iter()
                .filter(|span| span.text != EFFORT_SEPARATOR)
                .map(|span| span.text.clone())
                .collect::<Vec<_>>();

            for (index, label) in labels.iter().enumerate() {
                assert_eq!(
                    pick_on(&steps, label),
                    Some(Pick::Effort(index)),
                    "step {label} at width {width}"
                );
            }
            assert_eq!(pick_mid(&steps, EFFORT_SEPARATOR), None);
        }
    }

    #[test]
    fn hovering_a_selected_effort_lights_its_label_not_its_box_edges() {
        let slider = EffortSlider {
            efforts: ["low", "medium", "high"].map(ToOwned::to_owned).to_vec(),
            selected: 1,
            detail: None,
        };
        let steps = effort_step_lines(&slider, 80).remove(1);
        let selected = "│ MEDIUM │";
        let painted_steps = painted(&steps);
        let selected_start =
            UnicodeWidthStr::width(&painted_steps[..painted_steps.find(selected).unwrap()]);

        assert_eq!(
            Renderer::hover_columns(&steps, None, Some(&Pick::Effort(1))),
            Some(selected_start + 2..selected_start + 8)
        );
    }

    /// The top row of a picker, closable or not, at 80 columns.
    fn picker_top_row(closable: bool) -> PaintLine {
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable,
                title: "Model".to_owned(),
                lines: vec![OverlayLine {
                    text: "1. GPT-5.6 Sol".to_owned(),
                    selected: true,
                    muted: false,
                }],
                slider: None,
                hint: "Enter select".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        frame.lines.into_iter().next().expect("the top row")
    }

    /// The mark sits inside the corner with a stroke of rule between, and answers
    /// for the close — including the column either side of it.
    #[test]
    fn a_closable_picker_wears_the_mark_just_inside_its_corner() {
        let row = picker_top_row(true);
        let painted = painted(&row);

        assert!(painted.ends_with(" X ─╮"), "{painted}");
        assert_eq!(pick_on(&row, "X"), Some(Pick::Close));
        // The mark and one blank either side of it, and nothing more.
        assert_eq!(
            Renderer::hover_columns(&row, None, Some(&Pick::Close)).map(|columns| columns.len()),
            Some(3)
        );
        // The box keeps its width: the mark is painted into the rule, not added to
        // it, so the corner stays where every other row's border is.
        assert_eq!(
            painted_line_width(&row),
            painted_line_width(&picker_top_row(false))
        );
    }

    /// A panel the user cannot close carries no mark to click.
    #[test]
    fn a_plain_picker_has_no_mark_and_nothing_to_close() {
        let row = picker_top_row(false);

        assert!(!painted(&row).contains(" X "));
        assert_eq!(row.pick, None);
    }

    /// The resume list is drawn as a compact panel rather than a picker, and wears
    /// the same mark on the same corner.
    #[test]
    fn the_compact_panel_carries_the_mark_too() {
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: true,
                title: "Resume session · 3 · this folder".to_owned(),
                lines: vec![OverlayLine {
                    text: "3m ago    Fixing the picker".to_owned(),
                    selected: true,
                    muted: false,
                }],
                slider: None,
                hint: "Enter resume".to_owned(),
                style: OverlayStyle::CompactPanel,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        let row = &frame.lines[0];

        assert!(painted(row).ends_with(" X ─╮"), "{}", painted(row));
        assert_eq!(pick_on(row, "X"), Some(Pick::Close));
        // The rule below the list keeps its own corner: one mark, on top.
        assert!(!frame.lines.iter().skip(1).any(|line| {
            line.pick
                .as_ref()
                .is_some_and(|regions| regions.columns_of(&Pick::Close).is_some())
        }));
    }

    /// The cells a painted row actually carries the hover tint on, read back off
    /// the escapes it printed.
    fn hovered_cells(painted: &str) -> String {
        let hover = theme::palette().hover_bg;
        let hover_escape = format!("48;2;{};{};{}", hover.0, hover.1, hover.2);
        let mut cells = String::new();
        let mut hovered = false;
        let mut chars = painted.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                if hovered {
                    cells.push(ch);
                }
                continue;
            }
            let mut sequence = String::new();
            for ch in chars.by_ref() {
                sequence.push(ch);
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
            if sequence.contains(&hover_escape) {
                hovered = true;
            } else if sequence.starts_with("[48;2;")
                || sequence == "[49m"
                || sequence == "[0m"
                || sequence.contains("[39m")
            {
                hovered = false;
            }
        }
        cells
    }

    /// The highlight is the clickable span and its one column of bleed — not the
    /// whole separator it reaches into, and not the border beside it.
    #[test]
    fn the_highlight_covers_exactly_the_clickable_columns() {
        theme::set_current(ThemeKind::Dark);
        let line = status_line_row(
            Some(StatusLineView {
                model: Some("GPT-5.6 Sol".to_owned()),
                effort: Some("high".to_owned()),
                context: None,
                five_hour_percent: None,
                five_hour_remaining: None,
                weekly_percent: Some(34),
                notice: None,
            }),
            "",
            120,
        );
        let hovered = Renderer::hover_columns(&line, None, Some(&Pick::Model)).expect("the model");
        let mut output = Vec::new();

        print_line_with_selection(&mut output, &line, None, Some(hovered)).expect("paint");

        let painted = String::from_utf8(output).expect("utf-8 escapes");
        assert_eq!(hovered_cells(&painted), " GPT-5.6 Sol ");
    }

    /// Only the piece under the pointer lights up, and it lights up wherever the
    /// row happens to have placed it.
    #[test]
    fn hovering_a_badge_lights_only_that_badge() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(80, "", Some(&mode));
        let text = painted(&line);
        let start = UnicodeWidthStr::width(&text[..text.find("Vibe: On").unwrap()]);

        // A column either side of the vibe label is part of the target.
        assert_eq!(
            Renderer::hover_columns(&line, None, Some(&Pick::VibeMode)),
            Some(start - 1..start + 9)
        );
        // Nothing on the rule answers for a pick it does not carry.
        assert_eq!(
            Renderer::hover_columns(&line, None, Some(&Pick::Model)),
            None
        );
        assert_eq!(Renderer::hover_columns(&line, None, None), None);
    }

    #[test]
    fn fast_badge_hover_keeps_a_trailing_click_cell() {
        theme::set_current(ThemeKind::Dark);
        let line = activity_line_with_composer_controls(
            PaintLine::blank(),
            &test_mode("Full Access", ModeAccent::Danger, false),
            None,
            120,
        )
        .expect("activity row has controls");
        let hovered = Renderer::hover_columns(&line, None, Some(&Pick::FastMode))
            .expect("fast badge is clickable");
        let text = painted(&line);
        let fast_end = UnicodeWidthStr::width(&text[..text.find("Fast: Off").unwrap()])
            + UnicodeWidthStr::width("Fast: Off");

        assert_eq!(hovered.end, fast_end + 1);
    }

    /// A long activity label crowds the controls off the active row. They belong
    /// on the composer rule then — dropping them for the whole turn would leave
    /// the vibe and permission badges with nothing to click.
    #[test]
    fn a_crowded_activity_row_leaves_the_controls_on_the_composer_rule() {
        theme::set_current(ThemeKind::Dark);
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let mut crowded = PaintLine::blank();
        crowded.text = "x".repeat(110);

        assert!(activity_line_with_composer_controls(crowded, &mode, None, 120).is_none());

        let rule = input_top_line(120, "", Some(&mode));
        assert_eq!(pick_on(&rule, "Vibe: On"), Some(Pick::VibeMode));
    }

    #[test]
    fn moving_between_badges_repaints_only_the_old_and_new_badge_cells() {
        // If this instead repaints 10..47, Shell/Diff/Panel would visibly flash
        // while the pointer moves from Response to Fast.
        assert_eq!(
            hover_repaint_columns(Some(10..26), Some(38..47)),
            vec![10..26, 38..47]
        );
    }

    #[test]
    fn hover_transition_does_not_reprint_unhovered_composer_badges() {
        let line = input_top_line(
            120,
            "",
            Some(&test_mode("Full Access", ModeAccent::Danger, true)),
        );
        let vibe = Renderer::hover_columns(&line, None, Some(&Pick::VibeMode)).expect("vibe badge");
        let fast = Renderer::hover_columns(&line, None, Some(&Pick::FastMode)).expect("fast badge");
        let mut output = Vec::new();

        for columns in hover_repaint_columns(Some(vibe), Some(fast.clone())) {
            print_line_columns(&mut output, &line, None, Some(fast.clone()), columns)
                .expect("partial repaint");
        }

        let painted = String::from_utf8(output).expect("utf-8 paint");
        assert!(painted.contains("Vibe: On"));
        assert!(painted.contains("Fast: On"));
        assert!(!painted.contains("View: Chat"));
    }

    /// A row offers its own text, so the highlight runs from the number to the end
    /// of the name and leaves both borders alone.
    #[test]
    fn hovering_a_picker_row_lights_its_text_and_not_the_borders() {
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "Model".to_owned(),
                lines: vec![OverlayLine {
                    text: "1. GPT-5.6 Sol".to_owned(),
                    selected: true,
                    muted: false,
                }],
                slider: None,
                hint: "Enter select".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        let row = frame
            .lines
            .iter()
            .find(|line| painted(line).contains("GPT-5.6 Sol"))
            .expect("the model row");

        let painted_row = painted(row);
        let start = UnicodeWidthStr::width(&painted_row[..painted_row.find('1').unwrap()]);
        let end = start + UnicodeWidthStr::width("1. GPT-5.6 Sol") + 1;
        assert_eq!(
            Renderer::hover_columns(row, None, Some(&Pick::Row(0))),
            Some(start - 1..end)
        );
        // Both borders stay outside the highlight even with the column of bleed.
        assert!(start > 1);
        assert!(end < painted_line_width(row));
        assert_eq!(
            Renderer::hover_columns(row, None, Some(&Pick::Row(1))),
            None
        );
    }

    /// A compact row offers the same span a taller picker's row does: its own
    /// text, with the marker gutter, the inset and both borders left out.
    #[test]
    fn hovering_a_compact_row_lights_its_text_and_not_the_furniture() {
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "Resume session".to_owned(),
                lines: vec![OverlayLine {
                    text: "2h ago    Fix the picker".to_owned(),
                    selected: true,
                    muted: false,
                }],
                slider: None,
                hint: "Enter resume".to_owned(),
                style: OverlayStyle::CompactPanel,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        let row = frame
            .lines
            .iter()
            .find(|line| painted(line).contains("Fix the picker"))
            .expect("the session row");

        let painted_row = painted(row);
        let start = UnicodeWidthStr::width(&painted_row[..painted_row.find('2').unwrap()]);
        assert_eq!(
            Renderer::hover_columns(row, None, Some(&Pick::Row(0))),
            Some(start..start + UnicodeWidthStr::width("2h ago    Fix the picker"))
        );
        // The border and the `❯` gutter lead the row; the inset and the closing
        // border trail it. None of them light up.
        assert!(start > 1);
        assert!(
            start + UnicodeWidthStr::width("2h ago    Fix the picker") < painted_line_width(row)
        );
    }

    /// The panel border shifts every column of the track along with it, so the
    /// step a click lands on is only right if the regions moved too.
    #[test]
    fn effort_steps_stay_clickable_once_the_panel_border_goes_on() {
        let frame = overlay_frame(
            &[],
            OverlayView {
                closable: false,
                title: "Effort".to_owned(),
                lines: Vec::new(),
                slider: Some(EffortSlider {
                    efforts: ["low", "medium", "high"].map(ToOwned::to_owned).to_vec(),
                    selected: 1,
                    detail: None,
                }),
                hint: "←→ to adjust".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            None,
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );
        let steps = frame
            .lines
            .iter()
            .find(|line| painted(line).contains("MEDIUM"))
            .expect("the track");

        assert_eq!(pick_on(steps, "low"), Some(Pick::Effort(0)));
        assert_eq!(pick_on(steps, "│ MEDIUM │"), Some(Pick::Effort(1)));
        assert_eq!(pick_on(steps, "high"), Some(Pick::Effort(2)));
        // The border the panel put on is not a step.
        assert_eq!(steps.pick.as_ref().and_then(|regions| regions.at(0)), None);
    }

    #[test]
    fn effort_steps_use_compact_unselected_labels_at_narrow_width() {
        let slider = EffortSlider {
            efforts: ["low", "medium", "high", "xhigh", "max", "ultra"]
                .map(ToOwned::to_owned)
                .to_vec(),
            selected: 2,
            detail: None,
        };

        let lines = effort_step_lines(&slider, 40);

        assert_eq!(painted(&lines[1]).trim(), "L › M › │ HIGH │ › XH › MAX › U");
    }

    #[test]
    fn effort_steps_handle_empty_efforts_and_a_stale_selection() {
        assert!(
            effort_step_lines(
                &EffortSlider {
                    efforts: Vec::new(),
                    selected: 0,
                    detail: None,
                },
                80,
            )
            .is_empty()
        );

        let slider = EffortSlider {
            efforts: ["low", "medium", "high", "xhigh", "max", "ultra"]
                .map(ToOwned::to_owned)
                .to_vec(),
            selected: 99,
            detail: None,
        };
        let lines = effort_step_lines(&slider, 80);
        let selected = lines[1]
            .tail
            .iter()
            .find(|span| span.bold)
            .expect("clamped selected effort");

        assert_eq!(selected.text, "│ ULTRA │");
        assert_eq!(selected.tone, Tone::EffortUltra);
    }

    #[test]
    fn model_families_have_distinct_consistent_tones() {
        let tones = [
            model_tone("GPT-5.6 Sol"),
            model_tone("GPT-5.6 Terra"),
            model_tone("GPT-5.6 Luna"),
            model_tone("GPT-5.5"),
        ];

        assert!(tones.iter().all(Option::is_some));
        for left in 0..tones.len() {
            for right in left + 1..tones.len() {
                assert!(tones[left] != tones[right]);
            }
        }
        assert!(model_tone("GPT-5.4").is_none());
    }

    #[test]
    fn claude_models_use_the_devez_code_colors_everywhere() {
        let palette = theme::palette();
        for (model, model_tone, status_tone, color) in [
            (
                "Claude Haiku 4.5",
                Tone::ModelHaiku,
                Tone::StatusModelHaiku,
                palette.status.model_haiku,
            ),
            (
                "Claude Sonnet 5",
                Tone::ModelSonnet,
                Tone::StatusModelSonnet,
                palette.status.model_sonnet,
            ),
            (
                "Claude Opus 5",
                Tone::ModelOpus,
                Tone::StatusModelOpus,
                palette.status.model_opus,
            ),
            (
                "Claude Fable 5",
                Tone::ModelFable,
                Tone::StatusModelFable,
                palette.status.model_fable,
            ),
        ] {
            assert_eq!(super::model_tone(model), Some(model_tone));
            assert_eq!(status_model_tone(model), Some(status_tone));
            assert_eq!(tone_rgb(model_tone), Some(color));
            assert_eq!(tone_rgb(status_tone), Some(color));
        }
    }

    #[test]
    fn side_panel_cycles_report_each_real_main_width() {
        let total = 141;
        let stages = [None, Some(48), Some(60), Some(72), None];
        let signals = stages.map(|panel_width| {
            let main_width = panel_width
                .and_then(|width| side_panel_layout(total, width))
                .map_or(total, |layout| layout.main_width as u16);
            devez_layout_signal(main_width)
        });

        assert_eq!(
            signals,
            [
                "\x1b]777;devez-layout-v1;141\x07",
                "\x1b]777;devez-layout-v1;92\x07",
                "\x1b]777;devez-layout-v1;80\x07",
                "\x1b]777;devez-layout-v1;68\x07",
                "\x1b]777;devez-layout-v1;141\x07",
            ]
        );
    }

    #[test]
    fn terra_uses_the_reference_green_model_colour() {
        assert_eq!(
            tone_rgb(Tone::ModelTerra),
            Some(theme::palette().model_terra)
        );
    }

    #[test]
    fn gpt56_and_spark_use_their_reference_model_colours() {
        assert_eq!(model_tone("GPT-5.6"), Some(Tone::Model56));
        assert_eq!(model_tone("GPT-5.3 Codex Spark"), Some(Tone::ModelSpark));
        assert_eq!(tone_rgb(Tone::Model56), Some(theme::palette().model_gpt56));
        assert_eq!(
            tone_rgb(Tone::ModelSpark),
            Some(theme::palette().model_spark)
        );
    }

    #[test]
    fn expanded_plan_summary_shows_all_steps_without_internal_toggle() {
        let summary = PlanSummary {
            explanation: None,
            steps: (1..=7)
                .map(|index| PlanStep {
                    text: format!("Task {index}"),
                    status: PlanStepStatus::Pending,
                    started_at: None,
                    elapsed: None,
                })
                .collect(),
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        };

        let lines = fixed_plan_summary_lines(&summary, 80, 0.0, true, None, None);

        assert_eq!(lines.len(), 12);
        assert!(painted(&lines[0]).starts_with("┌── Updated Plan · 0 / 7"));
        assert!(painted(&lines[0]).ends_with('┐'));
        assert!(lines[0].tail.iter().any(|span| span.text == " Alt + W "));
        assert!(lines[0].tail.iter().any(|span| span.tone == Tone::FastOff));
        assert!(lines[1].text.is_empty());
        assert!(painted(&lines[8]).contains("     Task 7"));
        assert!(!painted(&lines[8]).ends_with('┃'));
        assert!(lines[9].text.is_empty());
        assert!(painted(&lines[10]).starts_with('└'));
        assert!(painted(&lines[10]).ends_with('┘'));
        assert!(lines[11].text.is_empty());
    }

    /// Inside the docked panel the plan drops its card chrome: a heading, one
    /// blank row, then the steps, all painted at the panel's own left inset.
    #[test]
    fn the_side_panel_carries_the_plan_on_a_flat_surface() {
        let summary = PlanSummary {
            explanation: None,
            steps: vec![
                PlanStep {
                    text: "1. 첫 단계".to_owned(),
                    status: PlanStepStatus::Completed,
                    started_at: None,
                    elapsed: Some(Duration::from_secs(18)),
                },
                PlanStep {
                    text: "2. 두 번째 단계".to_owned(),
                    status: PlanStepStatus::InProgress,
                    started_at: None,
                    elapsed: Some(Duration::from_secs(7)),
                },
                PlanStep {
                    text: "3. 세 번째 단계".to_owned(),
                    status: PlanStepStatus::Pending,
                    started_at: None,
                    elapsed: None,
                },
            ],
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        };
        let layout =
            side_panel_layout(140, SIDE_PANEL_WIDTHS[0]).expect("140 columns carry the panel");
        let content = side_panel_plan_lines(&summary, layout.content_width(), 0.0, false);

        // The total rides on the heading only once every step is done, the same
        // rule the card's own total line follows.
        assert_eq!(painted(&content[0]), "▲ Updated Plan  1 / 3");
        assert_eq!(
            content[0].pick.as_ref().and_then(|picks| picks.at(0)),
            Some(Pick::PlanSummary)
        );
        assert!(content[1].text.is_empty());
        assert_eq!(content[2].prefix, "✔ ");
        assert_eq!(content[3].prefix, "▸ ");
        assert_eq!(content[4].prefix, "  ");
        assert!(painted(&content[2]).starts_with("✔ 1. 첫 단계"));
        assert!(painted(&content[2]).ends_with("(18s)"));
        assert!(painted(&content[3]).starts_with("▸ 2. 두 번째 단계"));
        assert!(content[5] == PaintLine::blank());
        assert_eq!(painted(&content[6]), "─".repeat(layout.content_width()));
        assert_eq!(content[6].tone, Tone::SidePanelDivider);
        assert!(
            content
                .iter()
                .all(|line| !painted(line).contains('┌') && !painted(line).contains('└')),
            "the panel content has no card border"
        );

        let mut frame = CellFrame::new(140, 9);
        paint_side_panel_into_frame(&mut frame, layout, 9, &content, None);

        let heading: String = (0..UnicodeWidthStr::width(UPDATED_PLAN_TITLE))
            .map(|offset| {
                frame
                    .cell(layout.content_left() + 2 + offset, 1)
                    .glyph
                    .clone()
            })
            .collect();
        assert_eq!(heading, UPDATED_PLAN_TITLE);
        let right = layout.panel_left + layout.panel_width - 1;
        for row in [1, 7] {
            assert_eq!(frame.cell(layout.panel_left, row).glyph, " ");
            assert_eq!(frame.cell(right, row).glyph, " ");
            assert_eq!(
                frame.cell(layout.panel_left, row).style.background,
                Some(theme::palette().hover_bg)
            );
            assert_eq!(
                frame.cell(right, row).style.background,
                Some(theme::palette().hover_bg)
            );
        }
        assert_eq!(
            frame.cell(layout.content_left(), 7).style.foreground,
            tone_rgb(Tone::SidePanelDivider)
        );

        let finished = PlanSummary {
            steps: summary
                .steps
                .iter()
                .map(|step| PlanStep {
                    status: PlanStepStatus::Completed,
                    elapsed: step.elapsed.or(Some(Duration::from_secs(60))),
                    ..step.clone()
                })
                .collect(),
            ..summary
        };
        let waiting = side_panel_plan_lines(&finished, layout.content_width(), 0.0, true);
        assert_eq!(painted(&waiting[0]), "▲ Updated Plan  2 / 3 진행 중");
        assert_ne!(waiting[4].prefix, "✔ ");
        assert_eq!(waiting[4].prefix_tone, Tone::Accent);
        assert_eq!(waiting[4].tone, Tone::Accent);
        assert!(waiting.iter().all(|line| !painted(line).contains('⏱')));

        let waiting_card = fixed_plan_summary_lines(&finished, 80, 0.0, true, None, None);
        assert!(painted(&waiting_card[0]).contains("Updated Plan · 2 / 3 진행 중"));
        assert_ne!(waiting_card[4].prefix, "  ✔  ");
        assert_eq!(waiting_card[4].prefix_tone, Tone::Accent);
        assert_eq!(waiting_card[4].tone, Tone::Accent);
        assert!(waiting_card.iter().all(|line| !painted(line).contains('⏱')));

        let done = side_panel_plan_lines(&finished, layout.content_width(), 0.0, false);
        assert_eq!(painted(&done[0]), "▲ Updated Plan  3 / 3  [⏱  1m 25s]");
        assert_eq!(done[4].prefix, "✔ ");
    }

    #[test]
    fn the_side_panel_lists_five_latest_prompts_with_requested_spacing() {
        let prompts = (1..=7)
            .map(|index| {
                Block::new(
                    BlockKind::User,
                    "Codex",
                    format!("prompt {index}\ncontinued"),
                )
            })
            .collect::<Vec<_>>();
        let expected = prompts
            .iter()
            .rev()
            .take(SIDE_PANEL_PROMPT_LIMIT)
            .map(Block::id)
            .collect::<Vec<_>>();
        let mut history = prompts;
        history.push(Block::new(BlockKind::Assistant, "Codex", "latest response"));
        let layout =
            side_panel_layout(140, SIDE_PANEL_WIDTHS[0]).expect("140 columns carry the panel");

        let lines = side_panel_prompt_lines(&history, layout.content_width(), true);

        assert_eq!(painted(&lines[0]), "▲ Input Prompt");
        assert_eq!(
            lines[0].pick.as_ref().and_then(|picks| picks.at(0)),
            Some(Pick::PromptSection)
        );
        assert!(lines[1].text.is_empty());
        assert_eq!(lines.len(), 2 + SIDE_PANEL_PROMPT_LIMIT + 2);
        for (offset, prompt_id) in expected.into_iter().enumerate() {
            let line = &lines[offset + 2];
            assert_eq!(
                line.pick.as_ref().and_then(|picks| picks.at(0)),
                Some(Pick::Prompt(prompt_id))
            );
            assert!(painted(line).contains(&format!("prompt {} continued", 7 - offset)));
        }
        assert!(lines[lines.len() - 2] == PaintLine::blank());
        assert_eq!(
            painted(lines.last().expect("prompt divider")),
            "─".repeat(layout.content_width())
        );
        assert_eq!(
            lines.last().expect("prompt divider").tone,
            Tone::SidePanelDivider
        );

        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.side_panel = Some(layout);
        renderer.side_panel_content = lines;
        assert_eq!(
            renderer.pick_at(layout.content_left() as u16, 3),
            Some(Pick::Prompt(history[6].id()))
        );
    }

    #[test]
    fn side_panel_section_titles_collapse_to_their_heading_and_divider() {
        let summary = PlanSummary {
            explanation: None,
            steps: vec![PlanStep {
                text: "1. hidden step".to_owned(),
                status: PlanStepStatus::Pending,
                started_at: None,
                elapsed: None,
            }],
            expanded: false,
            started_at: Instant::now(),
            elapsed: None,
        };
        let plan = side_panel_plan_lines(&summary, 44, 0.0, false);
        let prompts = side_panel_prompt_lines(
            &[Block::new(BlockKind::User, "Codex", "hidden prompt")],
            44,
            false,
        );

        assert_eq!(plan.len(), 3);
        assert_eq!(painted(&plan[0]), "▼ Updated Plan  0 / 1");
        assert_eq!(
            plan[0].pick.as_ref().and_then(|picks| picks.at(0)),
            Some(Pick::PlanSummary)
        );
        assert!(plan[1] == PaintLine::blank());
        assert_eq!(plan[2].tone, Tone::SidePanelDivider);
        assert_eq!(prompts.len(), 3);
        assert_eq!(painted(&prompts[0]), "▼ Input Prompt");
        assert_eq!(
            prompts[0].pick.as_ref().and_then(|picks| picks.at(0)),
            Some(Pick::PromptSection)
        );
        assert!(prompts[1] == PaintLine::blank());
        assert_eq!(prompts[2].tone, Tone::SidePanelDivider);
    }

    #[test]
    fn clickable_side_panel_text_uses_a_subdued_hover_surface() {
        let distance = |left: Rgb, right: Rgb| {
            u16::from(left.0.abs_diff(right.0))
                + u16::from(left.1.abs_diff(right.1))
                + u16::from(left.2.abs_diff(right.2))
        };
        for theme in ThemeKind::ALL {
            theme::set_current(theme);
            let palette = theme::palette();
            let hover = side_panel_hover_background();
            let divider = tone_rgb(Tone::SidePanelDivider).expect("divider color");
            assert_ne!(hover, palette.hover_bg);
            assert!(distance(palette.hover_bg, hover) < distance(palette.hover_bg, divider));
        }

        theme::set_current(ThemeKind::Dark);
        let layout =
            side_panel_layout(100, SIDE_PANEL_WIDTHS[0]).expect("100 columns carry the panel");
        let heading = side_panel_section_heading("Input Prompt", true, 44, Pick::PromptSection);
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.side_panel = Some(layout);
        renderer.side_panel_content = vec![heading.clone()];
        renderer.previous_lines = vec![PaintLine::blank(); 3];
        assert!(renderer.hover_at(layout.content_left() as u16, 1));
        assert_eq!(renderer.hovered_pick, Some(Pick::PromptSection));

        let mut frame = CellFrame::new(100, 3);
        paint_side_panel_row_into_frame(
            &mut frame,
            layout,
            1,
            1,
            3,
            std::slice::from_ref(&heading),
            None,
            &[],
            Some(&Pick::PromptSection),
        );
        let heading_width = painted_line_width(&heading);
        assert_eq!(
            frame.cell(layout.content_left(), 1).style.background,
            Some(side_panel_hover_background())
        );
        assert_eq!(
            frame
                .cell(layout.content_left() + heading_width, 1)
                .style
                .background,
            Some(theme::palette().hover_bg)
        );
    }

    #[test]
    fn a_side_panel_prompt_uses_one_clickable_row_and_ellipsizes() {
        let prompt = Block::new(
            BlockKind::User,
            "gpt-5.6-sol",
            "첫 번째 긴 내용 두 번째 긴 내용 세 번째 긴 내용 네 번째 긴 내용",
        );
        let prompt_id = prompt.id();
        let content_width = 16;

        let lines = side_panel_prompt_lines(&[prompt], content_width, true);

        assert_eq!(lines.len(), 5);
        assert_eq!(lines[2].prefix, "› ");
        assert!(painted(&lines[2]).ends_with('…'));
        assert_eq!(lines[2].prefix_tone, Tone::ModelSol);
        assert_eq!(lines[2].tone, Tone::Plain);
        assert_eq!(row_background(lines[2].tone), None);
        assert_eq!(bubble_background(&lines[2]), None);
        assert_eq!(
            lines[2].pick.as_ref().and_then(|picks| picks.at(0)),
            Some(Pick::Prompt(prompt_id))
        );
        assert!(painted_line_width(&lines[2]) <= content_width);
        assert!(lines[3] == PaintLine::blank());
        assert_eq!(lines[4].tone, Tone::SidePanelDivider);
    }

    #[test]
    fn side_panel_integrations_use_reference_status_marks() {
        let mut providers = vec![
            ProviderIntegrationView {
                provider: "Claude".to_owned(),
                enabled: false,
                active: false,
                mcp_expanded: true,
                plugins_expanded: true,
                mcp: None,
                plugins: None,
                mcp_error: None,
                plugin_error: None,
            },
            ProviderIntegrationView {
                provider: "Codex".to_owned(),
                enabled: true,
                active: true,
                mcp_expanded: true,
                plugins_expanded: true,
                mcp: Some(vec![
                    IntegrationItemView {
                        name: "context7".to_owned(),
                        state: IntegrationItemState::Active,
                        detail: "연결됨 · 도구 12".to_owned(),
                    },
                    IntegrationItemView {
                        name: "figma".to_owned(),
                        state: IntegrationItemState::Inactive,
                        detail: "로그인 필요".to_owned(),
                    },
                    IntegrationItemView {
                        name: "playwright".to_owned(),
                        state: IntegrationItemState::Pending,
                        detail: "연결 중".to_owned(),
                    },
                ]),
                plugins: Some(vec![]),
                mcp_error: None,
                plugin_error: None,
            },
        ];

        let lines = side_panel_integration_lines(&providers, 42, usize::MAX);

        assert_eq!(painted(&lines[0]), "▸ Codex  사용 중");
        assert_eq!(painted(&lines[1]), "▲ MCP");
        assert_eq!(
            lines[1].pick.as_ref().and_then(|picks| picks.at(0)),
            Some(Pick::McpSection("Codex".to_owned()))
        );
        assert_eq!(painted(&lines[2]), "● context7  연결됨 · 도구 12");
        assert_eq!(lines[2].prefix_tone, Tone::Success);
        assert_eq!(painted(&lines[3]), "× figma  로그인 필요");
        assert_eq!(lines[3].prefix_tone, Tone::Error);
        assert_eq!(painted(&lines[4]), "○ playwright  연결 중");
        assert_eq!(lines[4].prefix_tone, Tone::Warning);
        assert!(lines.iter().any(|line| painted(line) == "▲ Plugin"));
        assert!(
            lines
                .iter()
                .any(|line| painted(line) == "× Claude  연결 안 됨")
        );

        providers[1].mcp_expanded = false;
        let collapsed = side_panel_integration_lines(&providers, 42, usize::MAX);
        assert!(collapsed.iter().any(|line| painted(line) == "▼ MCP"));
        assert!(
            !collapsed
                .iter()
                .any(|line| painted(line).contains("context7"))
        );
    }

    #[test]
    fn side_panel_integrations_fit_the_remaining_rows() {
        let providers = vec![ProviderIntegrationView {
            provider: "Codex".to_owned(),
            enabled: true,
            active: true,
            mcp_expanded: true,
            plugins_expanded: true,
            mcp: Some(
                (1..=6)
                    .map(|index| IntegrationItemView {
                        name: format!("server-{index}"),
                        state: IntegrationItemState::Active,
                        detail: "연결됨".to_owned(),
                    })
                    .collect(),
            ),
            plugins: Some(vec![]),
            mcp_error: None,
            plugin_error: None,
        }];

        let lines = side_panel_integration_lines(&providers, 30, 4);

        assert_eq!(lines.len(), 4);
        assert_eq!(painted(&lines[0]), "▸ Codex  사용 중");
        assert_eq!(painted(&lines[1]), "▲ MCP");
        assert!(painted(lines.last().expect("overflow marker")).starts_with("… +"));
    }

    /// A step wider than the panel keeps one row of its own and ends in an
    /// ellipsis, so the steps below it never shift down.
    #[test]
    fn a_long_side_panel_step_stays_on_one_row() {
        let summary = PlanSummary {
            explanation: None,
            steps: vec![PlanStep {
                text: "1. 사이드패널 폭보다 훨씬 긴 작업 단계 제목을 넣어서 잘림을 확인한다"
                    .to_owned(),
                status: PlanStepStatus::Pending,
                started_at: None,
                elapsed: None,
            }],
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        };
        let layout =
            side_panel_layout(140, SIDE_PANEL_WIDTHS[0]).expect("140 columns carry the panel");
        let content = side_panel_plan_lines(&summary, layout.content_width(), 0.0, false);

        let rows = &content[2..content.len() - 2];
        assert_eq!(rows.len(), 1, "one step keeps one row before the divider");
        assert!(painted(&rows[0]).ends_with('…'));
        assert!(painted_line_width(&rows[0]) <= layout.content_width());
        assert_eq!(rows[0].prefix, "  ");
        assert!(content[content.len() - 2] == PaintLine::blank());
        assert_eq!(
            painted(content.last().unwrap()),
            "─".repeat(layout.content_width())
        );
        assert_eq!(content.last().unwrap().tone, Tone::SidePanelDivider);
    }

    #[test]
    fn expanded_plan_summary_header_has_a_collapse_mark() {
        let summary = PlanSummary {
            explanation: None,
            steps: vec![PlanStep {
                text: "Done".to_owned(),
                status: PlanStepStatus::Completed,
                started_at: None,
                elapsed: None,
            }],
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        };

        let lines = fixed_plan_summary_lines(&summary, 80, 0.0, false, None, None);

        assert!(painted(&lines[0]).ends_with(" Alt + W ▲ ─┐"));
        assert_eq!(UnicodeWidthStr::width(painted(&lines[0]).as_str()), 79);
        assert!(lines[0].tail.iter().any(|span| span.tone == Tone::FastOff));
        assert_eq!(pick_on(&lines[0], "▲"), Some(Pick::PlanSummary));
    }

    #[test]
    fn collapsed_plan_summary_shows_only_a_straight_header_with_expand_mark() {
        let summary = PlanSummary {
            explanation: None,
            steps: vec![PlanStep {
                text: "Task".to_owned(),
                status: PlanStepStatus::Pending,
                started_at: None,
                elapsed: None,
            }],
            expanded: false,
            started_at: Instant::now(),
            elapsed: None,
        };

        let lines = fixed_plan_summary_lines(&summary, 80, 0.0, false, None, None);

        assert_eq!(lines.len(), 2);
        assert!(painted(&lines[0]).starts_with("─── Updated Plan"));
        assert!(painted(&lines[0]).trim_end().ends_with("Alt + W ▼ ──"));
        assert_eq!(lines[0].tail[0].tone, Tone::FastOff);
        assert!(!painted(&lines[0]).contains(['┌', '┐']));
        assert_eq!(pick_on(&lines[0], UPDATED_PLAN_TITLE), None);
        assert_eq!(pick_on(&lines[0], "▼"), Some(Pick::PlanSummary));
        assert!(lines[1] == PaintLine::blank());
        assert_eq!(
            Renderer::hover_columns(&lines[0], None, Some(&Pick::PlanSummary))
                .map(|columns| columns.len()),
            Some(13)
        );
    }

    #[test]
    fn progress_group_folds_from_its_top_and_can_be_expanded_again() {
        let progress = Block::progress_group(vec![
            Block::new(BlockKind::Assistant, "Codex", "첫 진행 메시지"),
            Block::new(BlockKind::Assistant, "Codex", "두 번째 진행 메시지"),
        ]);

        let open = block_lines_with_mode_at(
            &progress,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Collapse,
            false,
            Some(1.0),
        );
        let folding = block_lines_with_mode_at(
            &progress,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Collapse,
            false,
            Some(0.4),
        );
        let closed = block_lines_with_mode_at(
            &progress,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Collapse,
            false,
            None,
        );
        let reopened = block_lines_with_mode_at(
            &progress,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Collapse,
            true,
            None,
        );

        assert!(open.len() > folding.len());
        assert!(folding.len() > closed.len());
        assert!(open[1] == PaintLine::blank());
        assert!(folding[1] == PaintLine::blank());
        assert!(matches!(folding[2].tone, Tone::ResponseTransition(_, _)));
        assert!(reopened.len() > closed.len());
        let reopened_text = reopened.iter().map(painted).collect::<Vec<_>>().join("\n");
        assert!(reopened_text.contains("첫 진행 메시지"));
        assert!(reopened_text.contains("두 번째 진행 메시지"));
        assert_eq!(closed[0].tool_heading, Some(progress.id()));

        let reopened_group = block_group_lines(
            &progress,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Collapse,
            true,
        );
        assert!(reopened_group.last() == Some(&PaintLine::blank()));
        assert!(
            reopened_group.get(reopened_group.len().saturating_sub(2)) != Some(&PaintLine::blank())
        );
    }

    #[test]
    fn expanded_history_shows_context_compaction_inside_the_group() {
        let progress = Block::progress_group(vec![
            Block::new(BlockKind::Assistant, "Codex", "첫 진행 메시지"),
            Block::new(BlockKind::System, "Context compacted", ""),
            Block::new(BlockKind::Assistant, "Codex", "두 번째 진행 메시지"),
        ]);

        let collapsed = block_group_lines(
            &progress,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Collapse,
            false,
        );
        let expanded = block_group_lines(
            &progress,
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Collapse,
            true,
        );

        assert!(
            !collapsed
                .iter()
                .any(|line| line.text == "Context compacted")
        );
        assert!(expanded.iter().any(|line| line.text == "Context compacted"));
    }

    #[test]
    fn steer_history_attaches_to_the_prompt_before_each_response_segment() {
        let first_prompt = Block::new(BlockKind::User, "Codex", "첫 요청");
        let first_prompt_id = first_prompt.id();
        let first_progress = Block::new(BlockKind::Assistant, "Codex", "첫 요청 진행 기록");
        let second_prompt = Block::new(BlockKind::User, "Codex", "추가 요청");
        let second_prompt_id = second_prompt.id();
        let second_progress = Block::new(BlockKind::Assistant, "Codex", "추가 요청 확인");
        let final_answer = Block::new(BlockKind::Assistant, "Codex", "최종 답변");
        let first_group = Block::progress_group(vec![first_progress.clone()]);
        let second_group = Block::progress_group(vec![second_progress.clone()]);
        let mut history = vec![
            first_prompt,
            first_progress,
            second_prompt,
            second_progress,
            final_answer,
        ];
        merge_history_block(&mut history, first_group);
        merge_history_block(&mut history, second_group);

        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.history = history;

        assert_eq!(
            renderer
                .progress_group_for_prompt(first_prompt_id)
                .expect("first prompt History")
                .children()[0]
                .body,
            "첫 요청 진행 기록"
        );
        assert_eq!(
            renderer
                .progress_group_for_prompt(second_prompt_id)
                .expect("steer prompt History")
                .children()[0]
                .body,
            "추가 요청 확인"
        );
    }

    #[test]
    fn history_toggle_anchors_the_current_transcript_height() {
        let progress = Block::progress_group(vec![
            Block::new(BlockKind::Assistant, "Codex", "첫 진행 메시지"),
            Block::new(BlockKind::Assistant, "Codex", "두 번째 진행 메시지"),
        ]);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.fold_progress_groups = true;
        renderer.history.push(progress);
        renderer.rewrap(80);
        renderer.previous_lines = renderer.wrapped.clone();
        renderer.last_transcript_rows = renderer.wrapped.len();
        let collapsed_rows = renderer.last_transcript_rows;

        assert!(renderer.toggle_tool_at(0));
        assert_eq!(renderer.history_view_rows_anchor, Some(collapsed_rows));
        assert!(renderer.wrapped.len() > collapsed_rows);
    }

    #[test]
    fn progress_group_ignores_double_click_word_selection() {
        let progress = Block::progress_group(vec![Block::new(
            BlockKind::Assistant,
            "Codex",
            "진행 메시지",
        )]);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.fold_progress_groups = true;
        renderer.expanded_tools.insert(progress.id());
        renderer.history.push(progress);
        renderer.rewrap(80);
        renderer.previous_lines = renderer.wrapped.clone();

        assert!(renderer.progress_group_rows[0].contains(&2));
        assert_eq!(renderer.double_click_word(3, 2), None);
        assert_eq!(renderer.double_click_word(3, 2), None);
        assert_eq!(renderer.selected_text(), None);
    }

    #[test]
    fn history_on_prompt_keeps_the_right_margin_and_hovers_the_whole_background() {
        theme::set_current(ThemeKind::Dark);
        let prompt = Block::new(
            BlockKind::User,
            "gpt-5.6-sol",
            "첫 번째 프롬프트 줄\n두 번째 프롬프트 줄",
        );
        let progress = Block::progress_group(vec![Block::new(
            BlockKind::Assistant,
            "Codex",
            "진행 메시지",
        )]);
        let progress_id = progress.id();
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.chat_layout = true;
        renderer.fold_progress_groups = true;
        renderer.history.extend([prompt, progress]);
        renderer.rewrap(80);

        let prompt_lines = renderer
            .wrapped
            .iter()
            .filter(|line| {
                line.pick.as_ref().is_some_and(|regions| {
                    regions
                        .0
                        .iter()
                        .any(|(_, _, pick)| *pick == Pick::History(progress_id))
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(prompt_lines.len(), 4);
        assert!(painted(prompt_lines.last().unwrap()).ends_with("History · 1  "));
        assert_eq!(painted_line_width(prompt_lines.last().unwrap()), 79);
        let history_span = prompt_lines
            .last()
            .unwrap()
            .tail
            .iter()
            .find(|span| span.text.starts_with("History"))
            .expect("History label has its own tone");
        assert_eq!(history_span.tone, Tone::History);
        assert_eq!(
            tone_rgb(history_span.tone),
            Some(blend(
                theme::palette().foreground,
                theme::palette().muted,
                HISTORY_LABEL_MUTED_BLEND
            ))
        );
        assert_ne!(
            tone_rgb(history_span.tone),
            Some(theme::palette().foreground)
        );
        assert_ne!(tone_rgb(history_span.tone), Some(theme::palette().muted));
        let history_row = renderer
            .wrapped
            .iter()
            .position(|line| painted(line).contains("History · 1"))
            .expect("History label is visible");
        assert!(renderer.wrapped.get(history_row + 1) == Some(&PaintLine::blank()));

        for line in prompt_lines {
            let hovered = Renderer::hover_columns(line, None, Some(&Pick::History(progress_id)))
                .expect("every prompt row is the History target");
            assert_eq!(hovered.end, 79);
            let mut idle_frame = CellFrame::new(80, 1);
            paint_line_into_frame(&mut idle_frame, 0, line, None, None, None);
            for column in hovered.clone() {
                assert_eq!(
                    idle_frame.cell(column, 0).style.background,
                    Some(theme::palette().user_prompt_bg)
                );
            }
            assert_eq!(idle_frame.cell(79, 0).style.background, None);

            let mut frame = CellFrame::new(80, 1);
            paint_line_into_frame(&mut frame, 0, line, None, Some(hovered.clone()), None);
            for column in hovered {
                assert_eq!(
                    frame.cell(column, 0).style.background,
                    Some(scroll_to_bottom_background(true))
                );
            }
            assert_eq!(frame.cell(79, 0).style.background, None);
        }
    }

    #[test]
    fn history_right_padding_survives_layout_and_width_changes() {
        let prompt = Block::new(BlockKind::User, "gpt-5.6-sol", "폭이 달라지는 프롬프트");
        let history_id = 99;

        for chat_layout in [false, true] {
            for width in [24, 40, 80, 120] {
                let lines = user_prompt_lines_with_history(
                    &prompt,
                    width,
                    Some((history_id, "History · 6", false)),
                    chat_layout,
                );
                let bottom = lines
                    .iter()
                    .find(|line| painted(line).contains("History · 6"))
                    .unwrap_or_else(|| {
                        panic!(
                            "prompt has History padding: chat_layout={chat_layout}, width={width}, lines={:?}",
                            lines.iter().map(painted).collect::<Vec<_>>()
                        )
                    });
                let right_padding = bottom
                    .tail
                    .last()
                    .map(|span| UnicodeWidthStr::width(span.text.as_str()))
                    .unwrap_or_default();
                let expected_padding = 2;
                assert_eq!(right_padding, expected_padding);
                assert!(
                    painted(bottom)
                        .ends_with(&format!("History · 6{}", " ".repeat(expected_padding)))
                );
                assert_eq!(painted_line_width(bottom), usize::from(width) - 1);
                assert!(lines.last() == Some(&PaintLine::blank()));
                assert_eq!(
                    bottom.pick.as_ref().and_then(|regions| {
                        regions
                            .columns_of(&Pick::History(history_id))
                            .map(|columns| columns.end)
                    }),
                    Some(usize::from(width) - 1)
                );
            }
        }
    }

    #[test]
    fn non_chat_history_hover_colours_the_space_after_the_model_border() {
        theme::set_current(ThemeKind::Dark);
        let prompt = Block::new(BlockKind::User, "gpt-5.6-sol", "보낸 프롬프트");
        let history_id = 99;
        let lines = user_prompt_lines_with_history(
            &prompt,
            80,
            Some((history_id, "History · 1", false)),
            false,
        );
        let line = lines
            .iter()
            .find(|line| painted(line).contains("History · 1"))
            .expect("History label is visible");
        let hovered = Renderer::hover_columns(line, None, Some(&Pick::History(history_id)))
            .expect("History hover covers the prompt");
        assert_eq!(hovered.start, 1);

        let mut frame = CellFrame::new(80, 1);
        paint_line_into_frame(&mut frame, 0, line, None, Some(hovered), None);
        assert_eq!(
            frame.cell(1, 0).style.background,
            Some(scroll_to_bottom_background(true))
        );
    }

    #[test]
    fn prompt_history_click_expands_without_double_click_selection() {
        let prompt = Block::new(BlockKind::User, "gpt-5.6-sol", "보낸 프롬프트");
        let progress = Block::progress_group(vec![Block::new(
            BlockKind::Assistant,
            "Codex",
            "펼쳐진 진행 메시지",
        )]);
        let progress_id = progress.id();
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.chat_layout = true;
        renderer.fold_progress_groups = true;
        renderer.history.extend([prompt, progress]);
        renderer.last_width = 80;
        renderer.rewrap(80);
        renderer.previous_lines = renderer.wrapped.clone();

        let (row, column) = renderer
            .previous_lines
            .iter()
            .enumerate()
            .find_map(|(row, line)| {
                let columns = line
                    .pick
                    .as_ref()?
                    .columns_of(&Pick::History(progress_id))?;
                Some((row as u16, columns.start as u16))
            })
            .expect("prompt carries the History target");
        assert_eq!(
            renderer.pick_at(column, row),
            Some(Pick::History(progress_id))
        );
        assert_eq!(renderer.double_click_word(column, row), None);
        assert_eq!(renderer.double_click_word(column, row), None);
        assert_eq!(renderer.selected_text(), None);

        assert!(renderer.toggle_tool(progress_id));
        assert_eq!(renderer.history_view_rows_anchor, None);
        assert_eq!(renderer.history_view_start_anchor, Some(0));
        let expanded = renderer
            .wrapped
            .iter()
            .map(painted)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("History · 1"));
        assert!(expanded.contains("펼쳐진 진행 메시지"));
        assert!(renderer.wrapped.last() == Some(&PaintLine::blank()));
        let view_rows = split_rows(30, 10, renderer.wrapped.len()).0;
        let start = renderer.wrapped.len().saturating_sub(view_rows);
        let visible_prompt_rows = renderer.wrapped[start..start + view_rows]
            .iter()
            .filter(|line| {
                line.pick.as_ref().is_some_and(|regions| {
                    regions.columns_of(&Pick::History(progress_id)).is_some()
                })
            })
            .count();
        assert_eq!(visible_prompt_rows, 3);
    }

    #[test]
    fn progress_group_is_a_disclosure_row_only_in_super_vibe() {
        let progress = Block::progress_group(vec![
            Block::new(BlockKind::Assistant, "Codex", "첫 진행 메시지"),
            Block::new(BlockKind::Assistant, "Codex", "두 번째 진행 메시지"),
        ]);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.history.push(progress);

        renderer.rewrap(80);
        let ordinary = renderer
            .wrapped
            .iter()
            .map(painted)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!ordinary.contains(HISTORY_TITLE));
        assert!(ordinary.contains("첫 진행 메시지"));
        assert!(ordinary.contains("두 번째 진행 메시지"));

        renderer.fold_progress_groups = true;
        renderer.rewrap(80);
        let super_vibe = renderer
            .wrapped
            .iter()
            .map(painted)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(super_vibe.contains("History · 2"));
        assert!(!super_vibe.contains("첫 진행 메시지"));
    }

    #[test]
    fn compact_plan_keeps_the_original_task_order() {
        let summary = PlanSummary {
            explanation: None,
            steps: vec![
                PlanStep {
                    text: "Task 1".to_owned(),
                    status: PlanStepStatus::Completed,
                    started_at: None,
                    elapsed: None,
                },
                PlanStep {
                    text: "Task 2".to_owned(),
                    status: PlanStepStatus::Pending,
                    started_at: None,
                    elapsed: None,
                },
                PlanStep {
                    text: "Task 3".to_owned(),
                    status: PlanStepStatus::Completed,
                    started_at: None,
                    elapsed: None,
                },
                PlanStep {
                    text: "Task 4".to_owned(),
                    status: PlanStepStatus::Pending,
                    started_at: None,
                    elapsed: None,
                },
                PlanStep {
                    text: "Task 5".to_owned(),
                    status: PlanStepStatus::Pending,
                    started_at: None,
                    elapsed: None,
                },
            ],
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        };

        let lines = fixed_plan_summary_lines(&summary, 80, 0.0, false, None, None);

        assert!(painted(&lines[2]).contains("  ✔  Task 1"));
        assert!(painted(&lines[3]).contains("     Task 2"));
        assert!(painted(&lines[4]).contains("  ✔  Task 3"));
        assert_eq!(lines[2].prefix_tone, Tone::FastOff);
        assert_eq!(lines[2].tone, Tone::PlanDone);
    }

    #[test]
    fn completed_plan_step_appends_elapsed_time_to_the_task() {
        let summary = PlanSummary {
            explanation: None,
            steps: vec![PlanStep {
                text: "Done".to_owned(),
                status: PlanStepStatus::Completed,
                started_at: None,
                elapsed: Some(Duration::from_secs(94)),
            }],
            expanded: true,
            started_at: Instant::now(),
            elapsed: Some(Duration::from_secs(94)),
        };

        let lines = fixed_plan_summary_lines(&summary, 80, 0.0, false, None, None);
        let elapsed_line = painted(&lines[3]);
        let bottom_border = painted(&lines[4]);

        assert!(painted(&lines[2]).contains("Done (1m 34s)"));
        assert!(elapsed_line.contains("⏱  1m 34s"));
        assert_eq!(
            UnicodeWidthStr::width(elapsed_line.as_str()),
            UnicodeWidthStr::width(bottom_border.as_str())
        );
        assert_eq!(
            UnicodeWidthStr::width(bottom_border.as_str()),
            panel_span(80)
        );
    }

    #[test]
    fn plan_row_full_repaint_skips_spinner_only_changes() {
        let mut previous = PaintLine::plain("작업");
        previous.prefix = "  ⠋  ".to_owned();
        previous.prefix_tone = Tone::Accent;
        previous.tone = Tone::Accent;
        let mut spinner = previous.clone();
        spinner.prefix = "  ⠙  ".to_owned();

        assert!(!plan_row_requires_full_repaint(&previous, &spinner));

        let mut completed = spinner.clone();
        completed.prefix_tone = Tone::FastOff;
        completed.tone = Tone::PlanDone;
        completed.tail.push(PaintSpan {
            text: " (5s)".to_owned(),
            tone: Tone::Muted,
            bold: false,
        });
        assert!(plan_row_requires_full_repaint(&spinner, &completed));
    }

    #[test]
    fn initial_plan_change_repaints_only_the_changed_step_row() {
        let mut before = PaintLine::plain("작업");
        before.prefix = "  ⠋  ".to_owned();
        before.prefix_tone = Tone::Accent;
        before.tone = Tone::Accent;
        let mut spinner = before.clone();
        spinner.prefix = "  ⠙  ".to_owned();
        let mut completed = spinner.clone();
        completed.prefix_tone = Tone::FastOff;
        completed.tone = Tone::PlanDone;
        completed.tail.push(PaintSpan {
            text: " (5s)".to_owned(),
            tone: Tone::Muted,
            bold: false,
        });

        assert_eq!(
            plan_rows_requiring_full_repaint(
                &[PaintLine::plain("header"), before],
                2,
                &[PaintLine::plain("header"), spinner.clone()],
                2,
            ),
            Vec::<usize>::new()
        );
        assert_eq!(
            plan_rows_requiring_full_repaint(
                &[PaintLine::plain("header"), spinner],
                2,
                &[PaintLine::plain("header"), completed],
                2,
            ),
            vec![1]
        );
    }

    #[test]
    fn in_progress_plan_step_uses_the_working_spinner_without_shimmer() {
        let summary = PlanSummary {
            explanation: None,
            steps: vec![PlanStep {
                text: "Working task".to_owned(),
                status: PlanStepStatus::InProgress,
                started_at: None,
                elapsed: None,
            }],
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        };
        let lines = fixed_plan_summary_lines(&summary, 80, 0.5, true, None, None);
        assert!(lines[2].prefix.contains('⠴'));
        assert_eq!(lines[2].text, "Working task");
        assert_eq!(lines[2].tone, Tone::Accent);
        assert_eq!(lines[2].prefix.chars().next(), Some(' '));
        assert!(lines[2].tail.is_empty());
    }

    #[test]
    fn active_plan_border_advances_effort_after_two_low_steps() {
        assert_eq!(plan_effort_tone(1), Tone::EffortLow);
        assert_eq!(plan_effort_tone(2), Tone::EffortLow);
        assert_eq!(plan_effort_tone(3), Tone::EffortMedium);
        assert_eq!(plan_effort_tone(4), Tone::EffortHigh);
        assert_eq!(plan_effort_tone(5), Tone::EffortXHigh);
        assert_eq!(plan_effort_tone(6), Tone::EffortMax);
        assert_eq!(plan_effort_tone(7), Tone::EffortUltra);
    }

    #[test]
    fn plan_update_shimmer_uses_the_active_request_effort() {
        let summary = PlanSummary {
            explanation: None,
            steps: (1..=7)
                .map(|index| PlanStep {
                    text: format!("Task {index}"),
                    status: PlanStepStatus::Pending,
                    started_at: None,
                    elapsed: None,
                })
                .collect(),
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        };

        let lines = fixed_plan_summary_lines(&summary, 80, 0.5, true, Some(0.5), Some("medium"));
        let shimmer_colours = lines[0]
            .tail
            .iter()
            .filter_map(|span| match span.tone {
                Tone::PlanShimmer(colour, _) => Some(colour),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(!shimmer_colours.is_empty());
        assert!(
            shimmer_colours
                .iter()
                .all(|colour| *colour == tone_rgb(Tone::EffortMedium).expect("effort colour"))
        );
    }

    #[test]
    fn plan_title_shimmer_moves_five_times_over_the_default_text() {
        assert_eq!(PLAN_SHIMMER_BAND, SHIMMER_BAND * 2.5);
        assert_eq!(PLAN_SHIMMER_LOOPS, 5.0);
        let title =
            plan_title_shimmer_spans("Updated Plan · 1 / 3", Some(0.125), Tone::EffortMedium);

        assert!(
            title
                .iter()
                .any(|span| matches!(span.tone, Tone::PlanShimmer(_, _)))
        );
        assert_eq!(
            plan_title_shimmer_spans("Updated Plan · 1 / 3", None, Tone::EffortMedium)[0].tone,
            Tone::Plain
        );
    }

    #[test]
    fn inactive_in_progress_plan_step_uses_a_static_triangle() {
        let summary = PlanSummary {
            explanation: None,
            steps: vec![PlanStep {
                text: "Paused task".to_owned(),
                status: PlanStepStatus::InProgress,
                started_at: None,
                elapsed: None,
            }],
            expanded: true,
            started_at: Instant::now(),
            elapsed: None,
        };

        let lines = fixed_plan_summary_lines(&summary, 80, 0.5, false, None, None);

        assert_eq!(lines[2].prefix, "  ▸  ");
        assert_eq!(lines[2].prefix_tone, Tone::Accent);
        assert!(lines[2].tail.is_empty());
    }

    #[test]
    fn gpt55_uses_its_theme_specific_model_colour() {
        assert_eq!(tone_rgb(Tone::Model55), Some(theme::palette().model_gpt55));
    }

    #[test]
    fn provider_handoff_keeps_conversation_and_work_but_drops_local_chrome() {
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.history = vec![
            Block::new(BlockKind::Welcome, "Welcome", "local"),
            Block::new(BlockKind::User, "Claude", "질문"),
            Block::new(BlockKind::Assistant, "Claude", "답변"),
            Block::new(BlockKind::Tool, "Shell", "cargo test"),
            Block::new(BlockKind::ModelChange, "Provider", "Codex"),
        ];

        let blocks = renderer.provider_handoff_blocks();

        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].kind, "user");
        assert_eq!(blocks[1].kind, "assistant");
        assert_eq!(blocks[2].kind, "tool");
        assert_eq!(renderer.last_history_block_id(), renderer.history[4].id());
    }

    #[test]
    fn panel_borders_use_the_theme_border_tone() {
        let panel_width = panel_span(80);
        let body = panel_line("row", panel_width, Tone::Plain, false);
        let bottom = panel_bottom(panel_width - 2);

        assert!(bottom.tone == Tone::Border);
        assert!(body.prefix_tone == Tone::Border);
        assert!(
            body.tail
                .first()
                .is_some_and(|span| span.tone == Tone::Border)
        );
    }
}
