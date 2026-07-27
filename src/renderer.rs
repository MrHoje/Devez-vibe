use std::{
    collections::HashSet,
    env, fs,
    io::{Stdout, Write, stdout},
    ops::Range,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
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
    editor::Editor,
    selection::{
        CellPosition, CellRange, CopyLine, Selection, SelectionFinish, extract_text,
        selected_char_count, selection_chunks,
    },
    state::{DiffDisplayMode, ShellDisplayMode},
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
    if let Some(value) = env::var("DEVEZ_RENDERER").ok().filter(|v| !v.is_empty()) {
        return RenderMode::parse(&value)
            .with_context(|| format!("DEVEZ_RENDERER 값을 알 수 없습니다: {value}"));
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
        .join("DevezCLI")
        .join("renderer.txt")
}

#[derive(Clone, Copy)]
pub enum BlockKind {
    Welcome,
    Update,
    User,
    Assistant,
    Reasoning,
    /// A `turn/plan/updated` snapshot. Its body is the encoded plan: `└ ` rows
    /// are the explanation, `✔ `/`▸ `/`□ ` rows are done/in-progress/pending
    /// steps. See [`plan_lines`].
    Plan,
    Tool,
    FileChange,
    Diff,
    ModelChange,
    Warning,
    Error,
    System,
}

#[derive(Clone)]
pub struct Block {
    id: u64,
    pub kind: BlockKind,
    pub title: String,
    pub body: String,
    children: Vec<Block>,
}

static NEXT_BLOCK_ID: AtomicU64 = AtomicU64::new(1);

impl Block {
    pub fn new(kind: BlockKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            id: NEXT_BLOCK_ID.fetch_add(1, Ordering::Relaxed),
            kind,
            title: title.into(),
            body: body.into(),
            children: Vec::new(),
        }
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn adopt_id(&mut self, source: &Self) {
        self.id = source.id;
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

    pub fn children(&self) -> &[Block] {
        &self.children
    }

    /// Credits come last so the variable-length list survives the round trip
    /// through [`BlockKind::Welcome`]'s newline-delimited body.
    pub fn welcome(plan: &str, cwd: &str, account: &str, credits: &[String]) -> Self {
        let mut body = format!("{plan}\n{cwd}\n{account}");
        for line in credits {
            body.push('\n');
            body.push_str(line);
        }
        Self::new(BlockKind::Welcome, "DEVEZ CLI", body)
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OverlayStyle {
    Panel,
    CompactPanel,
    Picker,
}

#[derive(Clone)]
pub struct OverlayLine {
    pub text: String,
    pub selected: bool,
    pub muted: bool,
}

pub struct WelcomeView {
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
    pub branch: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context: Option<String>,
    pub five_hour_percent: Option<u8>,
    pub weekly_percent: Option<u8>,
    pub reset_credits: Option<String>,
    pub notice: Option<String>,
}

/// Internal footer marker used when the user disables the status line entirely.
pub(crate) const HIDDEN_STATUS_LINE: &str = "\0";

/// How prominently a composer mode badge is painted on the composer top rule.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ModeAccent {
    Calm,
    Safe,
    Danger,
}

pub struct ComposerMode {
    pub label: String,
    pub accent: ModeAccent,
    pub response_length: String,
    pub fast_mode: bool,
    pub effort: String,
    pub shell_display_mode: String,
    pub diff_display_mode: String,
    pub info_panel_open: bool,
    /// What the thread is estimated to have cost so far. Absent before the first
    /// turn reports usage, and whenever the model has no published rate.
    pub cost: Option<String>,
}

pub struct View<'a> {
    pub live_blocks: Vec<Block>,
    pub overlay: Option<OverlayView<'a>>,
    /// A persistent right-hand panel, available only to the fullscreen renderer.
    pub info_panel_open: bool,
    pub editor: &'a Editor,
    pub composer_images: &'a [String],
    pub welcome: Option<WelcomeView>,
    pub suggestions: Vec<SuggestionView>,
    pub activity: Option<String>,
    /// The active turn's model identifier. A transient activity notice leaves
    /// this empty so it uses the ordinary foreground colour.
    pub activity_model: Option<String>,
    /// Where the `Working` shimmer is in its sweep, `0.0..1.0`.
    pub activity_phase: f32,
    pub footer: String,
    pub status_line: Option<StatusLineView>,
    pub composer_notice: Option<String>,
    pub composer_mode: Option<ComposerMode>,
    pub shell_display_mode: ShellDisplayMode,
    pub diff_display_mode: DiffDisplayMode,
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

pub struct Renderer {
    out: Stdout,
    mode: RenderMode,
    previous_lines: Vec<PaintLine>,
    cursor_line: usize,
    last_width: u16,
    last_height: u16,
    theme: ThemeKind,
    history: Vec<Block>,
    /// Rows the transcript is held back from its newest end. Zero follows the
    /// live output. Fullscreen only: inline scrolling belongs to the terminal.
    scroll_back: usize,
    /// The transcript already wrapped for `wrapped_width`. Fullscreen repaints
    /// the whole screen every keystroke, and re-wrapping the transcript each
    /// time would make typing cost O(transcript).
    wrapped: Vec<PaintLine>,
    /// User prompt rows in `wrapped`, used to keep the current scroll context
    /// visible after its original block has moved above the viewport.
    prompt_anchors: Vec<PromptAnchor>,
    wrapped_width: u16,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    expanded_tools: HashSet<u64>,
    hovered_tool: Option<u64>,
    painted_hovered_tool: Option<u64>,
    /// The clickable piece of chrome under the pointer, and the one the screen
    /// was last painted with, so only the rows whose highlight moved repaint.
    hovered_pick: Option<Pick>,
    painted_hovered_pick: Option<Pick>,
    selection: Selection,
    painted_selection: Option<CellRange>,
    painted_info_panel: Option<InfoPanelLayout>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct InfoPanelLayout {
    main_width: usize,
    panel_left: usize,
    panel_width: usize,
}

impl InfoPanelLayout {
    const HORIZONTAL_PADDING: usize = 2;

    fn content_left(self) -> usize {
        self.panel_left + Self::HORIZONTAL_PADDING
    }

    fn content_width(self) -> usize {
        self.panel_width - 2 * Self::HORIZONTAL_PADDING
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PromptAnchor {
    rows: Range<usize>,
    text: String,
}

impl PromptAnchor {
    fn new(rows: Range<usize>, text: impl Into<String>) -> Self {
        Self {
            rows,
            text: text.into(),
        }
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
            out: stdout(),
            mode,
            previous_lines: Vec::new(),
            cursor_line: 0,
            last_width: 0,
            last_height: 0,
            theme: selected_theme,
            history: Vec::new(),
            scroll_back: 0,
            wrapped: Vec::new(),
            prompt_anchors: Vec::new(),
            wrapped_width: 0,
            shell_display_mode: ShellDisplayMode::Collapse,
            diff_display_mode: DiffDisplayMode::Collapse,
            expanded_tools: HashSet::new(),
            hovered_tool: None,
            painted_hovered_tool: None,
            hovered_pick: None,
            painted_hovered_pick: None,
            selection: Selection::default(),
            painted_selection: None,
            painted_info_panel: None,
        }
    }

    pub const fn mode(&self) -> RenderMode {
        self.mode
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
            self.clear_selection();
        }
        moved
    }

    /// Returns the fullscreen transcript to its newest position. Inline mode
    /// leaves scrolling to the terminal's own scrollback.
    pub fn scroll_to_bottom(&mut self) -> bool {
        if self.mode != RenderMode::Fullscreen || self.scroll_back == 0 {
            return false;
        }
        self.scroll_back = 0;
        self.clear_selection();
        true
    }

    fn scroll_to_bottom_control(&self, width: u16) -> Option<PaintLine> {
        if self.mode != RenderMode::Fullscreen || self.scroll_back == 0 {
            return None;
        }
        let text = " Scroll to bottom (Ctrl+↓) ";
        let start = usize::from(width)
            .saturating_sub(UnicodeWidthStr::width(text))
            / 2;
        Some(
            PaintLine {
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
            },
        )
    }

    /// Rows a page key jumps. A row of overlap keeps a line of context in view.
    pub fn page_rows(&self) -> isize {
        // Floored at 3 above, so the overlap subtraction always leaves at least 1.
        self.last_height.max(3) as isize - 2
    }

    pub fn clear_screen(&mut self) -> Result<()> {
        self.history.clear();
        self.wrapped.clear();
        self.prompt_anchors.clear();
        self.wrapped_width = 0;
        self.scroll_back = 0;
        self.expanded_tools.clear();
        self.hovered_tool = None;
        self.painted_hovered_tool = None;
        self.hovered_pick = None;
        self.painted_hovered_pick = None;
        self.selection.clear();
        self.painted_selection = None;
        self.painted_info_panel = None;
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
        let columns = line.pick.as_ref()?.columns_of(hovered_pick?)?;
        // A whole-row region runs to the end of the painted row, not past it.
        Some(columns.start..columns.end.min(painted_line_width(line)))
    }

    pub fn begin_selection(&mut self, column: u16, row: u16) -> bool {
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
        match self.selection.finish(point) {
            SelectionFinish::Copy(range) => {
                let text = extract_text(&self.copy_lines(), range);
                if text.trim().is_empty() {
                    SelectionResult::None
                } else {
                    SelectionResult::Copy(text)
                }
            }
            SelectionFinish::Click(cell) => SelectionResult::Click(cell.column, cell.row),
            SelectionFinish::None => SelectionResult::None,
        }
    }

    pub fn clear_selection(&mut self) -> bool {
        self.selection.clear()
    }

    fn reconcile_selection(&mut self, lines: &[PaintLine]) {
        let Some(range) = self.selection.range() else {
            return;
        };
        let changed = (usize::from(range.start.row)..=usize::from(range.end.row))
            .any(|row| self.previous_lines.get(row) != lines.get(row));
        if changed {
            self.selection.clear();
        }
    }

    fn selection_point(&self, column: u16, row: u16) -> Option<CellPosition> {
        if self.mode != RenderMode::Fullscreen || self.previous_lines.is_empty() {
            return None;
        }
        let row = row.min(self.previous_lines.len().saturating_sub(1) as u16);
        let line = &self.previous_lines[usize::from(row)];
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
        Some(CellPosition { column, row })
    }

    fn copy_lines(&self) -> Vec<CopyLine> {
        self.previous_lines
            .iter()
            .map(|line| CopyLine {
                text: painted_line_text(line),
                join_next: copy_joins_next(line),
                marker_width: if is_copy_marker(&line.prefix) {
                    UnicodeWidthStr::width(line.prefix.as_str())
                } else {
                    0
                },
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
            let lines = block_group_lines(
                block,
                width,
                self.shell_display_mode,
                self.diff_display_mode,
                self.expanded_tools.contains(&block.id()),
            );
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
            replace_history_block(&mut self.history, block);
        }
    }

    fn reset_screen(&mut self) -> Result<()> {
        self.previous_lines.clear();
        self.hovered_tool = None;
        self.painted_hovered_tool = None;
        self.selection.clear();
        self.painted_selection = None;
        self.cursor_line = 0;
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
        let mode_changed = self.shell_display_mode != view.shell_display_mode
            || self.diff_display_mode != view.diff_display_mode;
        if mode_changed {
            self.shell_display_mode = view.shell_display_mode;
            self.diff_display_mode = view.diff_display_mode;
            self.wrapped_width = 0;
            if self.mode == RenderMode::Inline {
                self.relayout()?;
            }
        }
        let (width, height) = terminal_size().unwrap_or((100, 30));
        let width = width.max(20);
        let info_panel = (self.mode == RenderMode::Fullscreen && view.info_panel_open)
            .then(|| info_panel_layout(width))
            .flatten();
        let frame_width = info_panel
            .map(|layout| layout.main_width as u16)
            .unwrap_or(width);
        let status = StatusArea {
            fallback: view.footer,
            line: view.status_line,
            composer_notice: view.composer_notice,
            composer_mode: view.composer_mode,
        };
        let mut frame = if let Some(overlay) = view.overlay {
            overlay_frame_with_expansion(
                &view.live_blocks,
                overlay,
                view.welcome,
                status,
                frame_width,
                &self.expanded_tools,
                self.shell_display_mode,
                self.diff_display_mode,
            )
        } else {
            normal_frame_with_expansion(
                &view.live_blocks,
                view.editor,
                view.composer_images,
                view.welcome,
                &view.suggestions,
                view.activity.as_deref(),
                view.activity_model.as_deref(),
                view.activity_phase,
                status,
                frame_width,
                &self.expanded_tools,
                self.shell_display_mode,
                self.diff_display_mode,
            )
        };

        if self.mode == RenderMode::Fullscreen {
            return self.render_fullscreen(
                committed,
                frame,
                frame_width,
                width,
                height.max(3),
                info_panel,
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
            || hidden_thinking_merge;
        if needs_full_repaint {
            self.erase_live()?;
            if hidden_thinking_merge || inline_history_replacement {
                self.record_inline_history(committed);
                self.relayout()?;
            } else {
                for block in visible_transcript_blocks(
                    committed,
                    self.shell_display_mode,
                    self.diff_display_mode,
                ) {
                    let lines = block_group_lines(
                        block,
                        frame_width,
                        self.shell_display_mode,
                        self.diff_display_mode,
                        false,
                    );
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
        self.last_width = width;
        self.last_height = height;
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
        info_panel: Option<InfoPanelLayout>,
    ) -> Result<()> {
        let rows = height as usize;
        let old_view_rows = split_rows(rows, frame.lines.len(), self.wrapped.len()).0;
        self.commit_fullscreen_blocks(committed, width, old_view_rows);
        let provisional_view_rows = split_rows(rows, frame.lines.len(), self.wrapped.len()).0;
        self.scroll_back = self
            .scroll_back
            .min(self.wrapped.len().saturating_sub(provisional_view_rows));
        let (view_rows, live_rows) = split_rows(rows, frame.lines.len(), self.wrapped.len());
        // Padding the live frame is what puts the composer on the bottom row
        // *without* dragging the welcome card and the live blocks down with it:
        // `fit_frame` inserts the blanks at the dock, above the composer.
        fit_frame(&mut frame, live_rows);
        let max_back = self.wrapped.len() - view_rows;
        self.scroll_back = self.scroll_back.min(max_back);
        let normal_start = max_back - self.scroll_back;
        let mut sticky = (self.scroll_back > 0)
            .then(|| sticky_prompt_for_viewport(&self.prompt_anchors, normal_start, width))
            .flatten();
        let transcript_rows = view_rows.saturating_sub(usize::from(sticky.is_some()));
        let max_back = self.wrapped.len() - transcript_rows;
        self.scroll_back = self.scroll_back.min(max_back);
        let start = max_back - self.scroll_back;
        if sticky.is_some() {
            sticky = sticky_prompt_for_viewport(&self.prompt_anchors, start, width);
        }
        let (mut screen, cursor_line) = compose_screen(
            &self.wrapped,
            frame.lines,
            view_rows,
            start,
            frame.cursor_line,
            sticky,
        );
        let scroll_to_bottom_overlay = self.scroll_to_bottom_control(width).and_then(|control| {
            let row = scroll_to_bottom_overlay_row(view_rows, frame.composer_index)?;
            let line = screen.get_mut(row)?;
            let start = UnicodeWidthStr::width(control.prefix.as_str());
            let end = start + UnicodeWidthStr::width(control.text.as_str());
            match line.pick.as_mut() {
                Some(picks) => picks.0.insert(0, (start, end, Pick::ScrollToBottom)),
                None => line.pick = Some(PickRegions::span(start, end, Pick::ScrollToBottom)),
            }
            Some((row, control))
        });
        if let Some(layout) = info_panel {
            debug_assert_eq!(
                usize::from(total_width),
                layout.main_width
                    + INFO_PANEL_GAP
                    + layout.panel_width
                    + INFO_PANEL_AUTOWRAP_GUARD
            );
        }

        self.reconcile_selection(&screen);
        self.paint_screen(
            &screen,
            cursor_line,
            frame.cursor_col,
            frame.show_cursor,
            info_panel,
            scroll_to_bottom_overlay.as_ref().map(|(row, control)| (*row, control)),
        )?;
        self.previous_lines = screen;
        self.cursor_line = cursor_line;
        self.last_width = width;
        self.last_height = height;
        self.out.flush()?;
        Ok(())
    }

    fn commit_fullscreen_blocks(&mut self, committed: &[Block], width: u16, view_rows: usize) {
        if self.wrapped_width != width {
            let before = self.wrapped.len();
            for block in committed.iter().cloned() {
                replace_history_block(&mut self.history, block);
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
                        .flat_map(|existing| {
                            block_group_lines(
                                existing,
                                width,
                                self.shell_display_mode,
                                self.diff_display_mode,
                                self.expanded_tools.contains(&existing.id()),
                            )
                        })
                        .count();
                    let changed_end = changed_start
                        + block_group_lines(
                            &self.history[index],
                            width,
                            self.shell_display_mode,
                            self.diff_display_mode,
                            self.expanded_tools.contains(&block.id()),
                        )
                        .len();
                    changed_start..changed_end
                });
            replace_history_block(&mut self.history, block.clone());

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
            } else {
                let start = self.wrapped.len();
                let lines = block_group_lines(
                    block,
                    width,
                    self.shell_display_mode,
                    self.diff_display_mode,
                    self.expanded_tools.contains(&block.id()),
                );
                let end = start + lines.len().saturating_sub(1);
                self.wrapped.extend(lines);
                if matches!(block.kind, BlockKind::User) && start < end {
                    self.prompt_anchors
                        .push(PromptAnchor::new(start..end, prompt_anchor_text(block)));
                }
                if self.scroll_back > 0 {
                    let row_delta = self.wrapped.len() as isize - before as isize;
                    self.scroll_back = self.scroll_back.saturating_add_signed(row_delta);
                }
            }
        }
    }

    fn rewrap(&mut self, width: u16) {
        let mut wrapped = Vec::new();
        let mut prompt_anchors = Vec::new();
        for block in visible_transcript_blocks(
            &self.history,
            self.shell_display_mode,
            self.diff_display_mode,
        ) {
            let start = wrapped.len();
            let lines = block_group_lines(
                block,
                width,
                self.shell_display_mode,
                self.diff_display_mode,
                self.expanded_tools.contains(&block.id()),
            );
            let end = start + lines.len().saturating_sub(1);
            wrapped.extend(lines);
            if matches!(block.kind, BlockKind::User) && start < end {
                prompt_anchors.push(PromptAnchor::new(start..end, prompt_anchor_text(block)));
            }
        }
        self.wrapped = wrapped;
        self.prompt_anchors = prompt_anchors;
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
        info_panel: Option<InfoPanelLayout>,
        scroll_to_bottom_overlay: Option<(usize, &PaintLine)>,
    ) -> Result<()> {
        queue!(self.out, Hide)?;
        // A height change shifts every row's meaning, so the diff is void.
        let repaint_all = self.previous_lines.len() != lines.len() || self.painted_info_panel != info_panel;
        let mut main_rows_repainted = false;
        let selection = self
            .selection
            .range()
            .filter(|range| selection_is_worth_painting(*range, lines));
        for (row, line) in lines.iter().enumerate() {
            let hovered = Self::hover_columns(line, self.hovered_tool, self.hovered_pick.as_ref());
            let previously_hovered = self.previous_lines.get(row).and_then(|previous| {
                Self::hover_columns(
                    previous,
                    self.painted_hovered_tool,
                    self.painted_hovered_pick.as_ref(),
                )
            });
            let selected_columns =
                selection.and_then(|range| selection_columns_for_line(line, range, row));
            let previously_selected_columns = self.painted_selection.and_then(|range| {
                self.previous_lines
                    .get(row)
                    .and_then(|previous| selection_columns_for_line(previous, range, row))
            });
            let hover_only_repaint = !repaint_all
                && self.previous_lines.get(row) == Some(line)
                && previously_selected_columns == selected_columns
                && previously_hovered != hovered;
            if hover_only_repaint {
                for columns in hover_repaint_columns(previously_hovered, hovered.clone()) {
                    queue!(
                        self.out,
                        MoveTo(
                            columns.start.min(u16::MAX as usize) as u16,
                            row.min(u16::MAX as usize) as u16
                        )
                    )?;
                    print_line_columns(
                        &mut self.out,
                        line,
                        selected_columns.clone(),
                        hovered.clone(),
                        columns,
                    )?;
                }
                continue;
            }
            if !repaint_all
                && self.previous_lines.get(row) == Some(line)
                && previously_selected_columns == selected_columns
                && previously_hovered == hovered
            {
                continue;
            }
            if !repaint_all
                && previously_selected_columns == selected_columns
                && previously_hovered == hovered
                && let Some(columns) = self
                    .previous_lines
                    .get(row)
                    .and_then(|previous| shimmer_repaint_columns(previous, line))
            {
                queue!(
                    self.out,
                    MoveTo(
                        columns.start.min(u16::MAX as usize) as u16,
                        row.min(u16::MAX as usize) as u16
                    )
                )?;
                print_line_columns(&mut self.out, line, selected_columns, hovered, columns)?;
                continue;
            }
            if let Some(layout) = info_panel {
                let clear_range = info_panel_main_clear_range(self.painted_info_panel, layout);
                clear_main_row(&mut self.out, row, clear_range.end)?;
            } else {
                queue!(
                    self.out,
                    MoveTo(0, row.min(u16::MAX as usize) as u16),
                    Clear(ClearType::UntilNewLine)
                )?;
            }
            main_rows_repainted = true;
            print_line_with_selection(&mut self.out, line, selected_columns, hovered)?;
        }
        if let Some(layout) = info_panel
            && (repaint_all || main_rows_repainted)
        {
            paint_info_panel(&mut self.out, layout, lines.len())?;
        }
        if let Some((row, control)) = scroll_to_bottom_overlay {
            paint_scroll_to_bottom_overlay(&mut self.out, row, control)?;
        }
        self.painted_selection = selection;
        self.painted_hovered_tool = self.hovered_tool;
        self.painted_hovered_pick = self.hovered_pick.clone();
        self.painted_info_panel = info_panel;
        queue!(
            self.out,
            MoveTo(
                cursor_col.min(u16::MAX as usize) as u16,
                cursor_line.min(u16::MAX as usize) as u16
            )
        )?;
        if show_cursor {
            queue!(self.out, Show)?;
        }
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
        Ok(())
    }

    fn print_permanent(&mut self, block: &Block, lines: &[PaintLine]) -> Result<()> {
        let tagged = copy_metadata_applies(block.kind);
        for line in lines {
            if tagged {
                let marker_skip = usize::from(is_copy_marker(&line.prefix))
                    * UnicodeWidthStr::width(line.prefix.as_str());
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
        let old_len = self.previous_lines.len();
        let new_len = frame.lines.len();
        if old_len == 0 || new_len == 0 {
            self.erase_live()?;
            return self.print_frame_full(frame);
        }

        queue!(self.out, Hide)?;
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
                queue!(self.out, Clear(ClearType::CurrentLine))?;
                print_line(&mut self.out, &frame.lines[row])?;
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

fn info_panel_main_clear_range(
    painted_info_panel: Option<InfoPanelLayout>,
    layout: InfoPanelLayout,
) -> Range<usize> {
    0..if painted_info_panel != Some(layout) {
        layout.panel_left
    } else {
        layout.main_width
    }
}

const INFO_PANEL_WIDTH: usize = 24;
const INFO_PANEL_GAP: usize = 3;
const INFO_PANEL_MIN_MAIN_WIDTH: usize = 44;
/// Writing into a terminal's final cell can trigger an implicit wrap and move
/// the cursor before the next absolute paint command arrives.
const INFO_PANEL_AUTOWRAP_GUARD: usize = 1;

/// Leaves the conversation enough room to remain readable before docking the
/// fixed-width information panel at the right edge.
fn info_panel_content_width(width: u16) -> Option<u16> {
    let width = usize::from(width);
    (width
        >= INFO_PANEL_MIN_MAIN_WIDTH
            + INFO_PANEL_GAP
            + INFO_PANEL_WIDTH
            + INFO_PANEL_AUTOWRAP_GUARD)
        .then(|| {
            (width - INFO_PANEL_GAP - INFO_PANEL_WIDTH - INFO_PANEL_AUTOWRAP_GUARD) as u16
        })
}

fn info_panel_layout(total_width: u16) -> Option<InfoPanelLayout> {
    let main_width = usize::from(info_panel_content_width(total_width)?);
    Some(InfoPanelLayout {
        main_width,
        panel_left: main_width + INFO_PANEL_GAP,
        panel_width: INFO_PANEL_WIDTH,
    })
}

fn info_panel_row(row: usize, rows: usize, content_width: usize) -> String {
    let text = match row {
        0 => "Info panel",
        1 if rows > 1 => "No information yet",
        _ => "",
    };
    format!("{text:<content_width$}")
}

fn info_panel_paint_positions(
    layout: InfoPanelLayout,
    rows: usize,
) -> Vec<(usize, usize, usize, usize)> {
    let panel_left = layout.panel_left;
    let panel_width = layout.panel_width;
    let content = layout.content_left();
    (0..rows)
        .map(|row| (row, panel_left, panel_width, content))
        .collect()
}

/// Draw the panel only after conversation rows settle, so its fixed surface
/// never inherits a line's width, colour, or partial-repaint state.
fn paint_info_panel(out: &mut Stdout, layout: InfoPanelLayout, rows: usize) -> Result<()> {
    let background = blend(theme::palette().background, theme::palette().border, 72);
    for (row, panel_left, panel_width, content) in info_panel_paint_positions(layout, rows) {
        queue!(
            out,
            MoveTo(
                panel_left.min(u16::MAX as usize) as u16,
                row.min(u16::MAX as usize) as u16
            ),
            SetBackgroundColor(rgb_color(background)),
            Print(" ".repeat(panel_width)),
            MoveTo(
                content.min(u16::MAX as usize) as u16,
                row.min(u16::MAX as usize) as u16
            )
        )?;
        set_tone(out, Tone::Muted)?;
        queue!(
            out,
            Print(info_panel_row(row, rows, layout.content_width())),
            ResetColor
        )?;
    }
    Ok(())
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
    sticky: Option<PaintLine>,
) -> (Vec<PaintLine>, usize) {
    let start = start.min(wrapped.len());
    let transcript_rows = view_rows.saturating_sub(usize::from(sticky.is_some()));
    let end = (start + transcript_rows).min(wrapped.len());
    let mut screen = Vec::with_capacity(view_rows + live.len());
    if let Some(sticky) = sticky {
        screen.push(sticky);
    }
    screen.extend(wrapped[start..end].iter().cloned());
    // `split_rows` never asks for more transcript than there is, so this only
    // guards the invariant rather than laying anything out.
    screen.resize(view_rows, PaintLine::blank());
    let cursor_line = screen.len() + live_cursor_line;
    screen.extend(live);
    (screen, cursor_line)
}

/// The overlay floats three rows above the composer, covering only the button's
/// cells rather than claiming a transcript row of its own.
fn scroll_to_bottom_overlay_row(view_rows: usize, composer_index: Option<usize>) -> Option<usize> {
    composer_index.map(|index| view_rows + index.saturating_sub(4))
}

fn prompt_anchor_text(block: &Block) -> String {
    block
        .body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn sticky_prompt_for_viewport(
    anchors: &[PromptAnchor],
    viewport_start: usize,
    width: u16,
) -> Option<PaintLine> {
    let anchor = anchors
        .iter()
        .rev()
        .find(|anchor| anchor.rows.end <= viewport_start)?;
    let prefix = " ";
    Some(PaintLine {
        prefix: prefix.to_owned(),
        prefix_tone: Tone::Plain,
        text: compact_right(
            &anchor.text,
            usize::from(width).saturating_sub(UnicodeWidthStr::width(prefix)),
        ),
        tone: Tone::UserPrompt,
        bold: true,
        tool_heading: None,
        pick: None,
        tail: Vec::new(),
    })
}

fn move_to_row(out: &mut Stdout, current_row: &mut usize, target_row: usize) -> Result<()> {
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
    Context,
    StatusText,
    StatusSeparator,
    UserPrompt,
    ModelSol,
    ModelTerra,
    ModelLuna,
    Model55,
    Border,
    Branch,
    LimitFiveHour,
    LimitWeekly,
    ResetCredit,
    FastOn,
    FastOff,
    ModelChange,
    SyntaxComment,
    SyntaxString,
    SyntaxKeyword,
    SyntaxNumber,
    SyntaxType,
    SyntaxFunction,
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
    /// The response-length badge cycles the desired response length.
    ResponseLength,
    /// The shell-display badge: cycles between hidden, collapsed, and expanded output.
    ShellDisplayMode,
    /// The file-diff badge: cycles between hidden, summary, and full patch output.
    DiffDisplayMode,
    /// The information-panel badge: toggles the fullscreen side panel.
    InfoPanel,
    /// The `Fast: On`/`Fast: Off` badge: toggles the fast service tier.
    FastMode,
    /// The status line's model name: opens `/model`.
    Model,
    /// The status line's `eff:` reading: opens `/effort`.
    EffortSetting,
    /// The fullscreen transcript control that returns to its newest row.
    ScrollToBottom,
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
            .find(|(_, _, candidate)| candidate == pick)
            .map(|(start, end, _)| *start..*end)
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

    fn user_prompt_padding() -> Self {
        Self {
            tone: Tone::UserPrompt,
            ..Self::blank()
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
            if width > 0 {
                if let Some((_, pick)) = picks.iter().find(|(at, _)| *at == index) {
                    regions.push((
                        column.saturating_sub(PICK_BLEED),
                        column + width + PICK_BLEED,
                        pick.clone(),
                    ));
                }
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

/// One span per character, each lit by how close it is to the band's centre, so
/// the label carries a soft gradient instead of a hard block. `phase` runs
/// `0.0..1.0` across a single sweep: the band enters off the left edge and
/// leaves past the right one, which is why the travel spans the label plus a
/// band's width on either side.
fn shimmer_spans(label: &str, phase: f32, base: Rgb) -> Vec<PaintSpan> {
    let chars: Vec<char> = label.chars().collect();
    let travel = chars.len() as f32 + SHIMMER_BAND * 2.0;
    let centre = phase.clamp(0.0, 1.0) * travel - SHIMMER_BAND;
    chars
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let distance = (index as f32 - centre).abs() / SHIMMER_BAND;
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

const WORKING_SPINNER: [&str; 8] = ["✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳"];

fn activity_lines(
    activity: &str,
    activity_model: Option<&str>,
    phase: f32,
    width: u16,
) -> Vec<PaintLine> {
    let tone = activity_model.and_then(model_tone).unwrap_or(Tone::Plain);
    if UnicodeWidthStr::width(activity) > width as usize {
        return wrapped_line(" ", tone, activity, tone, false, width);
    }
    if let Some(trailer) = activity.strip_prefix("Working..") {
        let shimmer_base = tone_rgb(tone).unwrap_or(theme::palette().foreground);
        let mut tail = shimmer_spans("Working..", phase, shimmer_base);
        tail.push(PaintSpan {
            text: trailer.to_owned(),
            tone,
            bold: false,
        });
        return vec![PaintLine {
            prefix: " ".to_owned(),
            prefix_tone: tone,
            text: format!(
                "{} ",
                WORKING_SPINNER[(phase.clamp(0.0, 0.999) * WORKING_SPINNER.len() as f32) as usize]
            ),
            tone,
            bold: false,
            tool_heading: None,
            pick: None,
            tail,
        }];
    }
    if activity.starts_with("Completed (") {
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
    let mut tail = if glyph == "✓" {
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

/// Places the full composer control strip at the right edge of a one-line
/// activity row. A narrow terminal keeps the controls on the composer rule,
/// where they can use their existing progressive compression.
fn activity_line_with_composer_controls(
    mut line: PaintLine,
    mode: &ComposerMode,
    width: u16,
) -> Option<PaintLine> {
    // Match the status row below: its leading gutter leaves the terminal's
    // final column empty, so the two right-aligned controls share an edge.
    let right_edge = (width as usize).saturating_sub(2);
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
            .info_panel_index
            .map(|index| (badge_start + index, Pick::InfoPanel)),
    );
    picks.extend(
        badge
            .response_length_index
            .map(|index| (badge_start + index, Pick::ResponseLength)),
    );
    picks.extend(
        badge
            .fast_index
            .map(|index| (badge_start + index, Pick::FastMode)),
    );
    line.tail.push(rule_gap(gap));
    line.tail.extend(badge.spans);
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

/// Decorative conversation gutters identify a row's kind but are never part of
/// its text. They share the same range in selection paint and clipboard output.
fn selectable_content_columns(line: &PaintLine) -> Option<Range<usize>> {
    composer_content_columns(line).or_else(|| {
        let fallback_status_gutter =
            line.prefix == " " && line.prefix_tone == Tone::Muted && line.tone == Tone::Muted;
        let empty_gutter = !line.prefix.is_empty()
            && line.prefix.chars().all(|ch| ch == ' ')
            && !fallback_status_gutter;
        (is_copy_marker(&line.prefix) || empty_gutter)
            .then(|| UnicodeWidthStr::width(line.prefix.as_str())..painted_line_width(line))
    })
}

fn selection_columns_for_line(
    line: &PaintLine,
    range: CellRange,
    row: usize,
) -> Option<Range<usize>> {
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

    for row in usize::from(range.start.row)..=usize::from(range.end.row) {
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
    normal_frame_with_expansion(
        live,
        editor,
        &[],
        welcome,
        suggestions,
        activity,
        None,
        0.5,
        status,
        width,
        &HashSet::new(),
        ShellDisplayMode::Collapse,
        DiffDisplayMode::Collapse,
    )
}

#[allow(clippy::too_many_arguments)]
fn normal_frame_with_expansion(
    live: &[Block],
    editor: &Editor,
    composer_images: &[String],
    welcome: Option<WelcomeView>,
    suggestions: &[SuggestionView],
    activity: Option<&str>,
    activity_model: Option<&str>,
    activity_phase: f32,
    status: StatusArea,
    width: u16,
    expanded_tools: &HashSet<u64>,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
) -> Frame {
    let mut lines = Vec::new();
    if let Some(welcome) = welcome {
        lines.extend(welcome_lines(welcome, width));
        lines.push(PaintLine::blank());
    }

    for block in visible_transcript_blocks(live, shell_display_mode, diff_display_mode) {
        lines.extend(block_group_lines(
            block,
            width,
            shell_display_mode,
            diff_display_mode,
            expanded_tools.contains(&block.id()),
        ));
    }

    let mut dock_index = lines.len();
    let composer_mode = status.composer_mode.as_ref();
    let mut composer_controls_mode = composer_mode;
    let activity_uses_composer_spacer = activity.is_some() && suggestions.is_empty();
    // Transient rows stay in the pinned dock instead of scrolling away with the
    // conversation. Activity leads any command suggestions.
    if let Some(activity) = activity {
        if !matches!(lines.last(), Some(line) if line == &PaintLine::blank()) {
            lines.push(PaintLine::blank());
        }
        lines.push(PaintLine::blank());
        let mut activity_rows = activity_lines(activity, activity_model, activity_phase, width);
        if let Some(mode) = composer_controls_mode
            && let Some(row) = activity_line_with_composer_controls(
                activity_rows[0].clone(),
                mode,
                width,
            )
        {
            activity_rows[0] = row;
        }
        // Controls never return to the composer rule while activity is shown;
        // the active row keeps only the settings that fit beside its label.
        composer_controls_mode = None;
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
        lines.push(PaintLine::blank());
    }

    // Recalled history is labelled on the composer rule, so the position stays
    // visible for as long as the entry does.
    let recalled = editor
        .history_position()
        .map(|(position, total)| format!("{position}/{total}"))
        .unwrap_or_default();
    let (input_lines, input_cursor_line, input_cursor_col) = input_lines_with_controls(
        editor,
        composer_images,
        width,
        &recalled,
        "",
        status.composer_notice.as_deref(),
        composer_mode,
        composer_controls_mode,
    );
    let composer_index = lines.len();
    let cursor_line = composer_index + input_cursor_line;
    lines.extend(input_lines);
    if status.fallback != HIDDEN_STATUS_LINE {
        lines.push(status_line_row(status.line, &status.fallback, width));
    }

    Frame {
        lines,
        cursor_line,
        cursor_col: input_cursor_col,
        show_cursor: true,
        dock_index,
        composer_index: Some(composer_index),
    }
}

/// Narrowest inner width that still leaves both columns readable; below this the
/// welcome panel collapses to a single column.
const WELCOME_SPLIT_MIN: usize = 62;
const WELCOME_INFO_WIDTH: usize = 48;

fn welcome_lines(welcome: WelcomeView, width: u16) -> Vec<PaintLine> {
    let panel_width = panel_span(width);
    let inner_width = panel_width.saturating_sub(2);
    let left = welcome_info_rows(&welcome, inner_width);

    if inner_width < WELCOME_SPLIT_MIN {
        let mut lines = vec![panel_top(inner_width)];
        lines.extend(
            left.into_iter()
                .map(|(text, tone, bold)| panel_line(&text, panel_width, tone, bold)),
        );
        lines.push(panel_bottom(inner_width));
        return lines;
    }

    // Account and workspace information stays compact while release notes use
    // every additional cell available in a wide terminal.
    let left_width = WELCOME_INFO_WIDTH;
    let right_width = inner_width - left_width - 1;
    let left = welcome_info_rows(&welcome, left_width);
    let right = welcome_notes_rows(right_width);

    let mut lines = vec![PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: format!("╭{}┬{}╮", "─".repeat(left_width), "─".repeat(right_width)),
        tone: Tone::Border,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: Vec::new(),
    }];
    for row in 0..left.len().max(right.len()) {
        lines.push(split_panel_line(
            left.get(row),
            left_width,
            right.get(row),
            right_width,
        ));
    }
    lines.push(PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: format!("╰{}┴{}╯", "─".repeat(left_width), "─".repeat(right_width)),
        tone: Tone::Border,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: Vec::new(),
    });
    lines
}

type PanelRow = (String, Tone, bool);

/// Columns taken by the two-space margin plus the widest row label.
const WELCOME_LABEL_WIDTH: usize = 11;

fn welcome_info_rows(welcome: &WelcomeView, column_width: usize) -> Vec<PanelRow> {
    let mut rows = vec![
        (
            format!(
                "  ✦  DEVEZ CLI  v{}  with Codex",
                crate::update::CURRENT_VERSION
            ),
            Tone::Accent,
            true,
        ),
        (String::new(), Tone::Plain, false),
        (format!("  Plan     {}", welcome.plan), Tone::Plain, false),
    ];

    // First credit row sits beside the label; the rest hang under the value column.
    let mut credits = welcome.credits.iter();
    rows.push((
        format!("  Resets   {}", credits.next().map_or("—", String::as_str)),
        Tone::Plain,
        false,
    ));
    rows.extend(credits.map(|line| {
        (
            format!("{}{line}", " ".repeat(WELCOME_LABEL_WIDTH)),
            Tone::Muted,
            false,
        )
    }));

    rows.extend([
        (
            format!("  Account  {}", welcome.account),
            Tone::Plain,
            false,
        ),
        (
            format!(
                "  Folder   {}",
                compact_text(
                    &welcome.cwd,
                    column_width.saturating_sub(WELCOME_LABEL_WIDTH)
                )
            ),
            Tone::Plain,
            false,
        ),
        (String::new(), Tone::Plain, false),
        ("  /help commands".to_owned(), Tone::Muted, false),
    ]);
    rows
}

/// Release notes, wrapped to the column so long lines fold instead of truncating.
fn welcome_notes_rows(column_width: usize) -> Vec<PanelRow> {
    let mut rows = vec![
        ("  What's new".to_owned(), Tone::Accent, true),
        (String::new(), Tone::Plain, false),
    ];
    if crate::update::RELEASE_NOTES.is_empty() {
        rows.push(("  —".to_owned(), Tone::Muted, false));
        return rows;
    }
    let indent = 2;
    let options = textwrap::Options::new(column_width.saturating_sub(indent + 1).max(8))
        .break_words(false)
        .subsequent_indent("  ")
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties);
    for note in crate::update::RELEASE_NOTES {
        rows.extend(textwrap::wrap(note, &options).into_iter().map(|folded| {
            (
                format!("{}{folded}", " ".repeat(indent)),
                Tone::Muted,
                false,
            )
        }));
    }
    rows
}

/// One body row of the split welcome panel: `│ left │ right │`.
fn split_panel_line(
    left: Option<&PanelRow>,
    left_width: usize,
    right: Option<&PanelRow>,
    right_width: usize,
) -> PaintLine {
    let (left_text, left_tone, left_bold) = column_cell(left, left_width);
    let (right_text, right_tone, right_bold) = column_cell(right, right_width);
    PaintLine {
        prefix: "│".to_owned(),
        prefix_tone: Tone::Border,
        text: left_text,
        tone: left_tone,
        bold: left_bold,
        tool_heading: None,
        pick: None,
        tail: vec![
            PaintSpan {
                text: "│".to_owned(),
                tone: Tone::Border,
                bold: false,
            },
            PaintSpan {
                text: right_text,
                tone: right_tone,
                bold: right_bold,
            },
            PaintSpan {
                text: "│".to_owned(),
                tone: Tone::Border,
                bold: false,
            },
        ],
    }
}

/// Pads a row to exactly `width` columns so the panel borders stay aligned.
fn column_cell(row: Option<&PanelRow>, width: usize) -> PanelRow {
    // One column is held back so content never kisses the divider.
    let content_width = width.saturating_sub(1);
    let (text, tone, bold) = match row {
        // Head-first truncation: rows that need their tail (paths) arrive pre-compacted.
        Some((text, tone, bold)) => (compact_right(text, content_width), *tone, *bold),
        None => (String::new(), Tone::Plain, false),
    };
    let padding = width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
    (format!("{text}{}", " ".repeat(padding)), tone, bold)
}

/// Panels share the composer's span so their borders line up with the rule.
fn panel_span(width: u16) -> usize {
    (width as usize).saturating_sub(1).max(20)
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

fn panelize_content_line(mut line: PaintLine, panel_width: usize) -> PaintLine {
    line.prefix.insert(0, '│');
    line.prefix_tone = Tone::Border;
    // The border pushes every column of the row along with it, and the clickable
    // spans were measured before it went on.
    line.pick = line.pick.map(|regions| regions.shifted(1));
    close_panel_row(line, panel_width)
}

fn panel_top(inner_width: usize) -> PaintLine {
    PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: format!("╭{}╮", "─".repeat(inner_width)),
        tone: Tone::Border,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: Vec::new(),
    }
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

/// Full-width update banner: a rule, the headline, the hint, and a closing rule.
fn update_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let rule_width = (width as usize).max(1);
    let text_width = rule_width.saturating_sub(2);
    let rule = || PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: "─".repeat(rule_width),
        tone: Tone::Border,
        bold: false,
        tool_heading: None,
        pick: None,
        tail: Vec::new(),
    };
    vec![
        rule(),
        PaintLine {
            prefix: "● ".to_owned(),
            prefix_tone: Tone::Accent,
            text: compact_text(&block.title, text_width),
            tone: Tone::Accent,
            bold: true,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        },
        PaintLine {
            prefix: "  ".to_owned(),
            prefix_tone: Tone::Muted,
            text: compact_text(&block.body, text_width),
            tone: Tone::Muted,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        },
        rule(),
        PaintLine::blank(),
    ]
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
    overlay_frame_with_expansion(
        live,
        overlay,
        welcome,
        status,
        width,
        &HashSet::new(),
        ShellDisplayMode::Collapse,
        DiffDisplayMode::Collapse,
    )
}

fn overlay_frame_with_expansion(
    live: &[Block],
    overlay: OverlayView<'_>,
    welcome: Option<WelcomeView>,
    status: StatusArea,
    width: u16,
    expanded_tools: &HashSet<u64>,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
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
    for block in visible_transcript_blocks(live, shell_display_mode, diff_display_mode) {
        lines.extend(block_group_lines(
            block,
            width,
            shell_display_mode,
            diff_display_mode,
            expanded_tools.contains(&block.id()),
        ));
    }
    let dock_index = lines.len();

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
                    let tone = if row.muted {
                        Tone::Muted
                    } else if part.contains('●') && part.contains('○') {
                        Tone::Accent
                    } else {
                        model_tone(part).unwrap_or(Tone::Plain)
                    };
                    let wrapped = wrapped_line_with_continuation(
                        prefix,
                        "│   ",
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
                    // Reserve the closing border before wrapping, not after.
                    let wrapped = wrapped_line_with_continuation(
                        prefix,
                        "│   ",
                        Tone::Border,
                        part,
                        if row.muted { Tone::Muted } else { Tone::Plain },
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
    }
    let mut cursor_line = lines.len() - 1;
    let mut cursor_col = 0;
    let mut composer_index = None;
    let show_cursor = if let Some(editor) = overlay.input {
        // The composer rule reads as part of the picker without this gap.
        lines.push(PaintLine::blank());
        let (input, input_cursor_line, input_cursor_col) = input_lines(
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
    }
    frame.cursor_line = frame.cursor_line.min(frame.lines.len().saturating_sub(1));
}

fn status_line_row(status: Option<StatusLineView>, fallback: &str, width: u16) -> PaintLine {
    let Some(status) = status else {
        return PaintLine {
            prefix: " ".to_owned(),
            prefix_tone: Tone::Muted,
            text: compact_right(fallback, width.saturating_sub(1) as usize),
            tone: Tone::Muted,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        };
    };

    let mut spans = Vec::new();
    let mut picks = Vec::new();
    if let Some(branch) = status.branch.filter(|branch| !branch.is_empty()) {
        push_status_span(&mut spans, compact_right(&branch, 24), Tone::Branch);
    }
    if let Some(model) = status.model.filter(|model| !model.is_empty()) {
        let span = push_status_span(
            &mut spans,
            compact_right(&model, 28),
            model_tone(&model).unwrap_or(Tone::StatusText),
        );
        picks.push((span, Pick::Model));
    }
    if let Some(effort) = status.effort.filter(|effort| !effort.is_empty()) {
        let span = push_status_span(
            &mut spans,
            format!("eff: {effort}"),
            effort_tone(&effort).unwrap_or(Tone::StatusText),
        );
        picks.push((span, Pick::EffortSetting));
    }
    if let Some(context) = status.context.filter(|context| !context.is_empty()) {
        push_status_span(&mut spans, context, Tone::Context);
    }
    // The 5h window is dropped entirely when unknown rather than shown as a stub.
    if let Some(percent) = status.five_hour_percent {
        push_status_span(&mut spans, format!("5h: {percent}%"), Tone::LimitFiveHour);
    }
    if let Some(percent) = status.weekly_percent {
        push_status_span(&mut spans, format!("week: {percent}%"), Tone::LimitWeekly);
    }
    if let Some(reset) = status.reset_credits.filter(|reset| !reset.is_empty()) {
        push_status_span(&mut spans, reset, Tone::ResetCredit);
    }
    // Fast: On/Off lives on the composer top rule beside the permission mode.
    if let Some(notice) = status.notice.filter(|notice| !notice.is_empty()) {
        push_status_span(&mut spans, notice, Tone::Muted);
    }
    // Align with the activity controls above by keeping two blank terminal
    // columns to the right of the status line.
    let max_width = width.saturating_sub(3) as usize;
    let shortcut_hint = "Shift + ↑↓ model · ←→ effort";
    let content_width = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum::<usize>();
    let hint_width = UnicodeWidthStr::width(shortcut_hint);
    let hint_omitted = content_width + hint_width > max_width;
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
    trim_status_spans(&mut spans, max_width, hint_omitted);

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

/// Reset credits are actionable and compact, so on a narrow terminal the longer
/// context gauge yields before the `week`/`reset` pair at the right end.
const STATUS_TRUNCATION_MARKER: &str = "...";

fn trim_status_spans(spans: &mut Vec<PaintSpan>, max_width: usize, hint_omitted: bool) {
    let width = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum::<usize>();
    let mut trimmed = hint_omitted;
    if width > max_width
        && spans.iter().any(|span| span.tone == Tone::ResetCredit)
        && let Some(context) = spans.iter().position(|span| span.tone == Tone::Context)
    {
        let separator = context.saturating_sub(1);
        spans.drain(separator..=context);
        trimmed = true;
    }
    let width = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum::<usize>();
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
    blocks: &'a [Block],
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
) -> Vec<&'a Block> {
    let mut visible: Vec<&Block> = Vec::with_capacity(blocks.len());
    for block in blocks {
        if is_empty_thinking_block(block) {
            continue;
        }
        if shell_display_mode == ShellDisplayMode::Hide
            && (is_bash_block(block)
                || is_running_shell_anchor(block)
                || is_web_search_block(block))
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
        history
            .iter()
            .any(|existing| existing.id() == incoming.id())
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
            .saturating_sub(UnicodeWidthStr::width(marker))
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

/// Codex paints a plan update as `• Updated Plan`, the explanation hanging off
/// a `└`, then one checkbox row per step indented four columns: `✔` for done,
/// `□` for the rest, with the in-progress step lit instead of dimmed. The body
/// carries `▸` for that step so the row keeps a status the text alone can't.
fn plan_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let mut lines = wrapped_line("• ", Tone::Plain, &block.title, Tone::Plain, true, width);
    let mut steps = 0usize;
    for row in block.body.lines().filter(|row| !row.trim().is_empty()) {
        // The checkbox itself is never struck through, only the step behind it.
        let (prefix, tone, bold, text) = if let Some(rest) = row.strip_prefix("└ ") {
            ("  └ ", Tone::Muted, false, rest)
        } else if let Some(rest) = row.strip_prefix("✔ ") {
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

/// Reasoning summaries use a narrow `∴` gutter and a single dim italic
/// paragraph. Plan blocks keep their heading and one physical row per step.
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
    block_lines_with_expansion(block, width, false)
}

fn block_group_lines(
    block: &Block,
    width: u16,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    expanded: bool,
) -> Vec<PaintLine> {
    let mut lines = block_lines_with_mode(
        block,
        width,
        shell_display_mode,
        diff_display_mode,
        expanded,
    );
    while matches!(lines.last(), Some(line) if line == &PaintLine::blank()) {
        lines.pop();
    }
    if !lines.is_empty() {
        lines.push(PaintLine::blank());
    }
    lines
}

fn block_lines_with_mode(
    block: &Block,
    width: u16,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    expanded: bool,
) -> Vec<PaintLine> {
    if is_bash_block(block) {
        return shell_group_lines(block, width, shell_display_mode, expanded);
    }
    if matches!(block.kind, BlockKind::Welcome) {
        let mut values = block.body.lines();
        let mut lines = welcome_lines(
            WelcomeView {
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
            DiffDisplayMode::Collapse if expanded => file_change_lines(block, width),
            DiffDisplayMode::Collapse => file_change_summary_lines(block, width),
            DiffDisplayMode::Expand if expanded => file_change_summary_lines(block, width),
            DiffDisplayMode::Expand => file_change_lines(block, width),
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
        BlockKind::Reasoning | BlockKind::Plan | BlockKind::Tool | BlockKind::FileChange => {
            unreachable!("handled above")
        }
        BlockKind::Assistant => ("● ", Tone::Accent),
        BlockKind::Diff => ("● ", Tone::Accent),
        BlockKind::Warning => ("▲ ", Tone::Warning),
        BlockKind::Error => ("✕ ", Tone::Error),
        BlockKind::System => ("◆ ", Tone::Muted),
    };

    let conversational = matches!(block.kind, BlockKind::Assistant);
    let mut first_content = conversational;
    let mut lines = if conversational {
        Vec::new()
    } else {
        wrapped_line(marker, tone, &block.title, Tone::Plain, true, width)
    };
    if block.body.is_empty() {
        if conversational {
            lines.extend(wrapped_line(marker, tone, "", Tone::Plain, false, width));
        }
        return lines;
    }

    let force_diff = matches!(block.kind, BlockKind::Diff);
    let mut code = false;
    let mut code_language = String::new();
    let mut code_frame_indent = String::new();
    let mut code_frame_width = width;
    let body_lines = block.body.lines().collect::<Vec<_>>();
    for (line_index, raw_line) in body_lines.iter().enumerate() {
        let trimmed = raw_line.trim_start();
        if let Some(language) = trimmed.strip_prefix("```") {
            let (prefix, prefix_tone) = if code {
                (code_frame_indent.clone(), Tone::Muted)
            } else {
                body_prefix(&mut first_content, marker, tone, "  ", Tone::Muted)
            };
            let text = if code {
                code_box_bottom(&prefix, code_frame_width)
            } else {
                let label = if language.trim().is_empty() {
                    "code"
                } else {
                    language.trim()
                };
                code_frame_width = code_box_width(&prefix, label, &body_lines[line_index + 1..], width);
                code_box_top(&prefix, label, code_frame_width)
            };
            lines.push(PaintLine {
                prefix: prefix.clone(),
                prefix_tone,
                text,
                tone: Tone::Muted,
                bold: false,
                tool_heading: None,
                pick: None,
                tail: Vec::new(),
            });
            if code {
                code_language.clear();
            } else {
                code_language = language.trim().to_ascii_lowercase();
                code_frame_indent = " ".repeat(UnicodeWidthStr::width(prefix.as_str()));
            }
            code = !code;
            continue;
        }

        if code {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  │ ", Tone::Muted);
            lines.extend(boxed_code_lines(
                &prefix,
                prefix_tone,
                raw_line,
                &code_language,
                code_frame_width,
            ));
        } else if force_diff {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  ", Tone::Muted);
            lines.extend(diff_line(&prefix, prefix_tone, raw_line, width));
        } else if trimmed.starts_with('#') {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  ", Tone::Muted);
            lines.extend(markdown_line(
                &prefix,
                prefix_tone,
                trimmed.trim_start_matches('#').trim_start(),
                Tone::Plain,
                true,
                width,
            ));
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  • ", Tone::Accent);
            lines.extend(markdown_line(
                &prefix,
                prefix_tone,
                item,
                Tone::Plain,
                false,
                width,
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
                width,
            ));
        } else {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  ", Tone::Muted);
            lines.extend(markdown_line(
                &prefix,
                prefix_tone,
                raw_line,
                Tone::Plain,
                false,
                width,
            ));
        }
    }
    if code {
        lines.push(PaintLine {
            prefix: code_frame_indent.clone(),
            prefix_tone: Tone::Muted,
            text: code_box_bottom(&code_frame_indent, code_frame_width),
            tone: Tone::Muted,
            bold: false,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        });
    }
    lines.push(PaintLine::blank());
    lines
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
    let mut lines = vec![PaintLine::user_prompt_padding()];
    if block.body.is_empty() {
        lines.extend(wrapped_line(
            " ",
            Tone::Plain,
            "",
            Tone::UserPrompt,
            true,
            width,
        ));
        lines.push(PaintLine::user_prompt_padding());
        return lines;
    }

    for raw_line in block.body.lines() {
        lines.extend(wrapped_line(
            " ",
            Tone::Plain,
            raw_line,
            Tone::UserPrompt,
            true,
            width,
        ));
    }
    lines.push(PaintLine::user_prompt_padding());
    lines
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
        if let Some((label, consumed)) = inline_link(rest) {
            push_highlight_span(&mut spans, &label, Tone::Accent, strong);
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

    styled_lines(prefix, prefix_tone, spans, tone, bold, width)
}

/// Collapses `[label](url)` — and the `![alt](url)` image form — down to the
/// label so a model's file links stop leaking raw markdown into the transcript.
/// A `:line` (or `:line:column`) tail on the target is worth reading, so it gets
/// grafted onto the label; the rest of the path is noise the label already says.
/// Returns the text to paint plus how many bytes of `rest` it consumed.
fn inline_link(rest: &str) -> Option<(String, usize)> {
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
    let target = &after_paren[..end];
    let consumed = usize::from(image) + 1 + close + 2 + end + 1;

    let mut text = label.to_owned();
    if text.is_empty() {
        text = target.to_owned();
    } else if let Some(suffix) = line_suffix(target)
        && !text.ends_with(&suffix)
    {
        text.push_str(&suffix);
    }
    Some((text, consumed))
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

/// A fenced block closes around its label or widest code row, without growing
/// past the terminal's available width.
fn code_box_width(prefix: &str, label: &str, rows: &[&str], terminal_width: u16) -> u16 {
    let widest_code = rows
        .iter()
        .take_while(|row| !row.trim_start().starts_with("```"))
        .map(|row| UnicodeWidthStr::width(*row))
        .max()
        .unwrap_or(0);
    let inner_width = (UnicodeWidthStr::width(label) + 5).max(widest_code + 4);
    (UnicodeWidthStr::width(prefix) + inner_width)
        .min(usize::from(terminal_width))
        .min(usize::from(u16::MAX)) as u16
}

fn code_box_top(prefix: &str, label: &str, width: u16) -> String {
    let inner_width = usize::from(width).saturating_sub(UnicodeWidthStr::width(prefix));
    let label = compact_right(label, inner_width.saturating_sub(5));
    let used = 5 + UnicodeWidthStr::width(label.as_str());
    format!("┌─ {label} {}┐", "─".repeat(inner_width.saturating_sub(used)))
}

fn code_box_bottom(prefix: &str, width: u16) -> String {
    let inner_width = usize::from(width).saturating_sub(UnicodeWidthStr::width(prefix));
    format!("└{}┘", "─".repeat(inner_width.saturating_sub(2)))
}

/// Code rows reserve their last cell for the closing rule, then pad their
/// highlighted contents to it. Continuations keep the left rule instead of
/// drifting into the assistant gutter.
fn boxed_code_lines(
    prefix: &str,
    prefix_tone: Tone,
    text: &str,
    language: &str,
    width: u16,
) -> Vec<PaintLine> {
    let content_width = width.saturating_sub(1);
    let mut lines = if matches!(language, "diff" | "patch") {
        diff_line(prefix, prefix_tone, text, content_width)
    } else {
        styled_lines(
            prefix,
            prefix_tone,
            highlight_code(text, language),
            Tone::Code,
            false,
            content_width,
        )
    };
    let continuation = format!(
        "{}│ ",
        " ".repeat(UnicodeWidthStr::width(prefix).saturating_sub(2))
    );
    for (index, line) in lines.iter_mut().enumerate() {
        if index > 0 {
            line.prefix = continuation.clone();
            line.prefix_tone = Tone::Muted;
        }
        let padding = usize::from(width).saturating_sub(painted_line_width(line) + 1);
        if padding > 0 {
            line.tail.push(PaintSpan {
                text: " ".repeat(padding),
                tone: Tone::Code,
                bold: false,
            });
        }
        line.tail.push(PaintSpan {
            text: "│".to_owned(),
            tone: Tone::Muted,
            bold: false,
        });
    }
    lines
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
    let available = (width as usize).saturating_sub(prefix_width).max(1);
    let mut rows: Vec<Vec<PaintSpan>> = vec![Vec::new()];
    let mut used = 0;

    for span in spans {
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
    let available = width.saturating_sub(prefix_width).max(4);
    let options = textwrap::Options::new(available)
        .break_words(true)
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties);
    let wrapped = textwrap::wrap(text, options);
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

/// Gutter glyphs that label a block instead of belonging to its text, so a copy
/// should start past them. Card corners (`╭─ `) are deliberately absent: they
/// frame a block rather than mark one, and trimming them would leave the copied
/// card missing only its top-left edge.
const COPY_MARKERS: [&str; 9] = [
    "● ", "• ", "  • ", "✻ ", "∴ ", "▲ ", "✕ ", "◆ ", "❯ ",
];

fn is_copy_marker(prefix: &str) -> bool {
    COPY_MARKERS.contains(&prefix)
}

fn composer_display(editor: &Editor, composer_images: &[String]) -> (String, usize) {
    let labels = (1..=composer_images.len())
        .map(|index| format!("[Image #{index}]"))
        .collect::<Vec<_>>();
    let prefix = if labels.is_empty() {
        String::new()
    } else {
        format!("{} ", labels.join(" "))
    };
    let cursor = prefix.chars().count() + editor.cursor();
    (format!("{prefix}{}", editor.text()), cursor)
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

fn input_lines(
    editor: &Editor,
    composer_images: &[String],
    width: u16,
    label: &str,
    placeholder: &str,
    notice: Option<&str>,
    mode: Option<&ComposerMode>,
) -> (Vec<PaintLine>, usize, usize) {
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
) -> (Vec<PaintLine>, usize, usize) {
    let (display, editor_cursor) = composer_display(editor, composer_images);
    let display_chars = display.chars().collect::<Vec<_>>();
    let panel_width = (width as usize).saturating_sub(1).max(16);
    let side_prefix = "│ ";
    let first_prefix = "> ";
    let continuation_prefix = "  ";
    let content_width = panel_width
        .saturating_sub(
            UnicodeWidthStr::width(side_prefix) + UnicodeWidthStr::width(first_prefix) + 1,
        )
        .max(4);
    let mut raw_rows = vec![String::new()];
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

        if ch == '\n' {
            raw_rows.push(String::new());
            row += 1;
            column = input_prefix_width;
            continue;
        }

        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        let content_column = column.saturating_sub(input_prefix_width);
        if content_column + ch_width > content_width && !raw_rows[row].is_empty() {
            raw_rows.push(String::new());
            row += 1;
            column = input_prefix_width;
            if index == editor.cursor() {
                cursor_row = row;
                cursor_column = column;
            }
        }
        raw_rows[row].push(ch);
        column += ch_width;
    }

    if editor_cursor == display_chars.len() {
        cursor_row = row;
        cursor_column = column;
    }

    let mut rows = Vec::with_capacity(raw_rows.len() + 2);
    rows.push(input_top_line_with_controls(
        panel_width,
        label,
        mode,
        controls_mode,
    ));
    let chrome_tone = composer_chrome_tone(mode);
    for (index, raw) in raw_rows.into_iter().enumerate() {
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
            tone: Tone::Plain,
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

    (rows, cursor_row + 1, cursor_column)
}

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
    let left = if label.is_empty() {
        String::new()
    } else {
        format!("{OPENING_RULE}{label} ")
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
                .info_panel_index
                .map(|index| (badge_start + index, Pick::InfoPanel)),
        );
        picks.extend(
            badge
                .response_length_index
                .map(|index| (badge_start + index, Pick::ResponseLength)),
        );
        picks.extend(
            badge
                .fast_index
                .map(|index| (badge_start + index, Pick::FastMode)),
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
    mode.and_then(|mode| effort_tone(&mode.effort))
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

/// Widest badge that fits in `budget`: estimated cost · response length · Shell
/// display mode · Diff display mode · information panel · fast flag. Tightening
/// drops cost, then optional trailing controls; response length remains. Parts
/// are never ellipsized — a half-written label or clipped price is worse than none.
fn fitting_badge_spans(mode: &ComposerMode, budget: usize) -> Option<BadgeSpans> {
    let response_length_span = PaintSpan {
        text: format!("Response: {}", mode.response_length),
        tone: Tone::Muted,
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

    let shell_display_mode_span = PaintSpan {
        text: format!("Shell: {}", mode.shell_display_mode),
        tone: Tone::Muted,
        bold: false,
    };
    let diff_display_mode_span = PaintSpan {
        text: format!("Diff: {}", mode.diff_display_mode),
        tone: Tone::Muted,
        bold: false,
    };
    let info_panel_span = PaintSpan {
        text: format!(
            "Panel: {}",
            if mode.info_panel_open {
                "Opened"
            } else {
                "Closed"
            }
        ),
        tone: Tone::Muted,
        bold: false,
    };

    // Brackets mark the cost as an aside rather than a setting like the two
    // badges beside it. The cost leads, so its separator trails it instead.
    let cost_spans = mode
        .cost
        .as_deref()
        .map(|cost| {
            vec![
                PaintSpan {
                    text: format!("[{cost}]"),
                    tone: Tone::Plain,
                    bold: false,
                },
                separator_span(),
            ]
        })
        .unwrap_or_default();

    // Widest first. The cost is the least important reading and goes first;
    // Fast precedes the optional panel control in every retained layout.
    let cost_width = cost_spans.len();
    let ladder = [
        BadgeSpans {
            spans: [
                cost_spans,
                vec![response_length_span.clone(), separator_span()],
                vec![shell_display_mode_span.clone(), separator_span()],
                vec![diff_display_mode_span.clone(), separator_span()],
                vec![fast_span.clone(), separator_span()],
                vec![info_panel_span.clone()],
            ]
            .concat(),
            response_length_index: Some(cost_width),
            shell_display_mode_index: Some(cost_width + 2),
            diff_display_mode_index: Some(cost_width + 4),
            info_panel_index: Some(cost_width + 8),
            fast_index: Some(cost_width + 6),
        },
        BadgeSpans {
            spans: [
                vec![response_length_span.clone(), separator_span()],
                vec![shell_display_mode_span.clone(), separator_span()],
                vec![diff_display_mode_span.clone(), separator_span()],
                vec![fast_span, separator_span()],
                vec![info_panel_span],
            ]
            .concat(),
            response_length_index: Some(0),
            shell_display_mode_index: Some(2),
            diff_display_mode_index: Some(4),
            info_panel_index: Some(8),
            fast_index: Some(6),
        },
        BadgeSpans {
            spans: vec![
                response_length_span.clone(),
                separator_span(),
                shell_display_mode_span,
                separator_span(),
                diff_display_mode_span,
            ],
            response_length_index: Some(0),
            shell_display_mode_index: Some(2),
            diff_display_mode_index: Some(4),
            info_panel_index: None,
            fast_index: None,
        },
        BadgeSpans {
            spans: vec![response_length_span],
            response_length_index: Some(0),
            shell_display_mode_index: None,
            diff_display_mode_index: None,
            info_panel_index: None,
            fast_index: None,
        },
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
    info_panel_index: Option<usize>,
    fast_index: Option<usize>,
}

fn separator_span() -> PaintSpan {
    PaintSpan {
        text: COMPOSER_BADGE_SEPARATOR.to_owned(),
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
        "low" => Tone::EffortLow,
        "medium" => Tone::EffortMedium,
        "high" => Tone::EffortHigh,
        "xhigh" => Tone::EffortXHigh,
        "max" => Tone::EffortMax,
        "ultra" => Tone::EffortUltra,
        _ => return None,
    })
}

fn rule_gap(width: usize) -> PaintSpan {
    PaintSpan {
        text: " ".repeat(width),
        tone: Tone::Muted,
        bold: false,
    }
}

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

/// Paints over a transcript row after it was drawn, preserving the surrounding
/// text while the control occupies only its own centred button cells.
fn paint_scroll_to_bottom_overlay(
    out: &mut impl Write,
    row: usize,
    control: &PaintLine,
) -> Result<()> {
    let background = word_background(control.tone).expect("scroll control background");
    let foreground = tone_rgb(control.tone).expect("scroll control foreground");
    queue!(
        out,
        MoveTo(
            UnicodeWidthStr::width(control.prefix.as_str()).min(u16::MAX as usize) as u16,
            row.min(u16::MAX as usize) as u16
        ),
        SetBackgroundColor(rgb_color(background)),
        SetForegroundColor(rgb_color(foreground)),
        Print(&control.text),
        ResetColor
    )?;
    Ok(())
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
        Tone::UserPrompt => palette.user_prompt_bg,
        Tone::ModelChange => palette.model_change_bg,
        Tone::DiffAdded | Tone::DiffAddedWord => palette.diff_add_bg,
        Tone::DiffRemoved | Tone::DiffRemovedWord => palette.diff_remove_bg,
        _ => return None,
    })
}

/// The stronger tint a single run gets on top of its row's band, for the words a
/// diff row actually changed.
fn word_background(tone: Tone) -> Option<Rgb> {
    let palette = theme::palette();
    Some(match tone {
        Tone::DiffAddedWord => palette.diff_add_word_bg,
        Tone::DiffRemovedWord => palette.diff_remove_word_bg,
        Tone::ScrollToBottom => palette.hover_bg,
        _ => return None,
    })
}

fn print_line_with_selection(
    out: &mut impl Write,
    line: &PaintLine,
    selected_columns: Option<Range<usize>>,
    hovered_columns: Option<Range<usize>>,
) -> Result<()> {
    let background = row_background(line.tone);
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
        word_background(line.prefix_tone).or(background),
    )?;
    print_hovered_chunks(
        out,
        &line.text,
        &mut column,
        selected_columns.as_ref(),
        hovered_columns.as_ref(),
        line.tone,
        line.bold,
        word_background(line.tone).or(background),
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
            word_background(span.tone).or(background),
        )?;
        queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    }
    if let Some(background) = background {
        queue!(
            out,
            SetBackgroundColor(rgb_color(background)),
            Clear(ClearType::UntilNewLine),
            ResetColor
        )?;
    }
    Ok(())
}

/// Repaints just `columns` of an otherwise unchanged row. This is used for a
/// hover transition, where clearing and rewriting the entire composer rule
/// makes every neighbouring badge visibly blink.
fn print_line_columns(
    out: &mut impl Write,
    line: &PaintLine,
    selected_columns: Option<Range<usize>>,
    hovered_columns: Option<Range<usize>>,
    columns: Range<usize>,
) -> Result<()> {
    let background = row_background(line.tone);
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
        word_background(line.prefix_tone).or(background),
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
        word_background(line.tone).or(background),
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
            word_background(span.tone).or(background),
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
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
    if model.contains("5.6") && model.contains("sol") {
        Some(Tone::ModelSol)
    } else if model.contains("5.6") && model.contains("terra") {
        Some(Tone::ModelTerra)
    } else if model.contains("5.6") && model.contains("luna") {
        Some(Tone::ModelLuna)
    } else if model.contains("5.5") {
        Some(Tone::Model55)
    } else {
        None
    }
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
        Tone::Context => palette.status.context,
        Tone::StatusText => palette.status.text,
        Tone::StatusSeparator => palette.status.separator,
        Tone::UserPrompt => palette.foreground,
        Tone::ModelSol => palette.orange,
        Tone::ModelTerra => palette.pink,
        Tone::ModelLuna => palette.purple,
        Tone::Model55 => palette.blue,
        Tone::Border => palette.border,
        Tone::Branch => palette.status.branch,
        Tone::LimitFiveHour => palette.status.five_hour,
        Tone::LimitWeekly => palette.status.weekly,
        Tone::ResetCredit => palette.orange,
        Tone::FastOn => palette.success,
        Tone::FastOff => palette.muted,
        Tone::ModelChange => palette.foreground,
        Tone::SyntaxComment => palette.syntax_comment,
        Tone::SyntaxString => palette.syntax_string,
        Tone::SyntaxKeyword => palette.syntax_keyword,
        Tone::SyntaxNumber => palette.syntax_number,
        Tone::SyntaxType => palette.syntax_type,
        Tone::SyntaxFunction => palette.syntax_function,
        Tone::InlineCode => palette.accent,
        // Claude Code paints diff rows with the default text colour and lets the
        // green/red background carry the added/removed signal, so the text stays
        // as readable as the rest of the transcript.
        Tone::DiffAdded | Tone::DiffRemoved | Tone::DiffAddedWord | Tone::DiffRemovedWord => {
            palette.foreground
        }
        Tone::DiffHeader => palette.diff_header,
        Tone::Shimmer(base, level) => blend(base, palette.foreground, level),
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

    #[test]
    fn info_panel_layout_keeps_a_gap_and_the_last_column_unpainted() {
        let layout = info_panel_layout(72).expect("72 columns fit the panel");

        assert_eq!(layout.main_width, 44);
        assert_eq!(layout.panel_left, 47);
        assert_eq!(layout.panel_width, 24);
        assert_eq!(layout.panel_left + layout.panel_width, 71);
    }

    #[test]
    fn info_panel_opening_clear_reaches_panel_but_steady_clear_stops_at_main_frame() {
        let layout = info_panel_layout(72).unwrap();
        let resized_layout = info_panel_layout(73).unwrap();

        assert_eq!(info_panel_main_clear_range(None, layout), 0..47);
        assert_eq!(info_panel_main_clear_range(Some(layout), layout), 0..44);
        assert_eq!(info_panel_main_clear_range(Some(layout), resized_layout), 0..48);
    }

    #[test]
    fn info_panel_content_is_inset_inside_the_panel_surface() {
        let layout = info_panel_layout(72).unwrap();

        assert_eq!(layout.content_left(), 49);
        assert_eq!(layout.content_width(), 20);
    }

    #[test]
    fn info_panel_paints_a_full_surface_with_inset_content() {
        let layout = info_panel_layout(72).unwrap();

        assert_eq!(
            info_panel_paint_positions(layout, 3),
            [(0, 47, 24, 49), (1, 47, 24, 49), (2, 47, 24, 49)]
        );
    }

    #[test]
    fn info_panel_text_has_no_box_drawing_vertical_rule() {
        let layout = info_panel_layout(72).unwrap();
        let row = info_panel_row(0, 3, layout.content_width());

        assert_eq!(UnicodeWidthStr::width(row.as_str()), 20);
        assert!(!row.contains('│'));
    }

    #[test]
    fn info_panel_reserves_the_last_terminal_column_against_autowrap() {
        assert_eq!(info_panel_content_width(71), None);
        assert_eq!(info_panel_content_width(72), Some(44));
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
        };

        fit_frame(&mut frame, 6);

        assert_eq!(frame.lines.len(), 6);
        assert_eq!(frame.cursor_line, 4);
        assert_eq!(frame.lines[0].text, "response");
        assert_eq!(frame.lines[4].text, "input");
        assert_eq!(frame.lines[5].text, "footer");
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

        let (rows, _, _) = input_lines(&editor, &[], 18, "", "placeholder", None, None);
        let prompt_rows = &rows[1..rows.len() - 1];

        assert!(prompt_rows.len() > 1);
        assert!(!rows[0].text.contains("Message"));
        assert_eq!(painted(&rows[0]), "╭───────────────╮");
        // Both rules are drawn in the same border colour the welcome card uses.
        assert!(rows[0].tone == Tone::Border);
        assert!(rows.last().is_some_and(|row| row.tone == Tone::Border));
        assert_eq!(painted(&prompt_rows[0]), "│ > wrapped-prom│");
        assert_eq!(painted(&prompt_rows[1]), "│   pt-text     │");
        assert_eq!(
            painted(rows.last().expect("bottom rule")),
            "╰───────────────╯"
        );
    }

    #[test]
    fn fullscreen_composer_copy_excludes_the_box_chrome() {
        let mut editor = Editor::default();
        editor.set_text("copy");
        let (rows, _, _) = input_lines(&editor, &[], 18, "", "placeholder", None, None);
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = rows;

        assert!(renderer.begin_selection(0, 1));
        assert!(renderer.update_selection(16, 1));
        assert_eq!(
            renderer.finish_selection(16, 1),
            SelectionResult::Copy("copy".to_owned())
        );
    }

    #[test]
    fn fullscreen_composer_highlight_excludes_the_box_chrome() {
        let mut editor = Editor::default();
        editor.set_text("copy");
        let (rows, _, _) = input_lines(&editor, &[], 18, "", "placeholder", None, None);
        let range = CellRange {
            start: CellPosition { column: 0, row: 1 },
            end: CellPosition { column: 16, row: 1 },
        };

        assert_eq!(selection_columns_for_line(&rows[1], range, 1), Some(4..8));
    }

    #[test]
    fn fullscreen_selection_excludes_response_and_thinking_gutters() {
        let assistant =
            block_lines(&Block::new(BlockKind::Assistant, "Codex", "answer"), 80).remove(0);
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
            Some(2..9)
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
            SelectionResult::Copy("thought".to_owned())
        );
    }

    #[test]
    fn fullscreen_selection_excludes_every_bullet_gutter() {
        let plan = block_lines(&Block::new(BlockKind::Plan, "Plan", "- first"), 80)
            .into_iter()
            .find(|line| line.prefix == "• ")
            .expect("plan bullet");
        let list = block_lines(&Block::new(BlockKind::System, "Notice", "- first"), 80)
            .into_iter()
            .find(|line| line.prefix == "  • ")
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
            Some(2..painted_line_width(&plan))
        );
        assert_eq!(
            selection_columns_for_line(&list, full_row(&list, 0), 0),
            Some(4..painted_line_width(&list))
        );
    }

    #[test]
    fn fullscreen_selection_excludes_blank_continuation_gutter_under_a_bullet() {
        let lines = wrapped_line(
            "  • ",
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
        let second_row = CellRange {
            start: CellPosition { column: 0, row: 1 },
            end: CellPosition { column: 7, row: 1 },
        };
        assert_eq!(selection_columns_for_line(&lines[1], second_row, 1), Some(2..8));
        let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
        renderer.previous_lines = lines;

        assert!(renderer.begin_selection(0, 0));
        assert!(renderer.update_selection(7, 1));
        assert_eq!(
            renderer.finish_selection(7, 1),
            SelectionResult::Copy("first\nsecond".to_owned())
        );
    }

    #[test]
    fn fullscreen_selection_excludes_the_blank_user_prompt_gutter() {
        let lines = block_lines(&Block::new(BlockKind::User, "You", "first\nsecond"), 80);
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
    fn user_prompt_group_has_inner_padding_above_and_below_the_prompt() {
        let lines = block_group_lines(
            &Block::new(BlockKind::User, "You", "first\nsecond"),
            80,
            ShellDisplayMode::Collapse,
            DiffDisplayMode::Expand,
            false,
        );

        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].tone, Tone::UserPrompt);
        assert_eq!(lines[0].text, "");
        assert_eq!(lines[1].text, "first");
        assert_eq!(lines[2].text, "second");
        assert_eq!(lines[3].tone, Tone::UserPrompt);
        assert_eq!(lines[3].text, "");
        assert!(lines[4] == PaintLine::blank());

        let selection = CellRange {
            start: CellPosition { column: 0, row: 1 },
            end: CellPosition { column: 8, row: 2 },
        };
        assert_eq!(selection_columns_for_line(&lines[0], selection, 0), None);
        assert_eq!(selection_columns_for_line(&lines[3], selection, 3), None);
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
        let images = vec![r"C:\Temp\clipboard-image.bmp".to_owned()];

        let (display, cursor) = composer_display(&editor, &images);

        assert_eq!(display, r"[Image #1] inspect C:\tmp\ordinary.png");
        assert_eq!(cursor, display.chars().count());
    }

    #[test]
    fn input_lines_show_image_labels_when_the_text_editor_is_empty() {
        let editor = Editor::default();
        let images = vec![r"C:\Temp\clipboard-image.bmp".to_owned()];

        let (rows, _, _) = input_lines(&editor, &images, 80, "", "", None, None);

        assert!(painted(&rows[1]).contains("> [Image #1] "));
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
    fn update_banner_spans_the_full_width_between_two_rules() {
        let block = Block::new(
            BlockKind::Update,
            "Update Available",
            "New version 0.11.9 is available. Run: dvz update",
        );
        let lines = block_lines(&block, 80);

        assert_eq!(lines.len(), 5);
        assert!(lines[0].text.chars().all(|ch| ch == '─'));
        assert_eq!(UnicodeWidthStr::width(lines[0].text.as_str()), 80);
        assert_eq!(lines[0].text, lines[3].text);
        assert_eq!(lines[1].prefix, "● ");
        assert_eq!(lines[1].prefix_tone, Tone::Accent);
        assert_eq!(lines[1].text, "Update Available");
        assert!(lines[1].bold);
        assert!(lines[2].text.contains("0.11.9"));
        assert!(lines[2].text.ends_with("dvz update"));
        assert!(lines[4].text.is_empty());
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
    fn powershell_code_block_closes_at_its_longest_line() {
        let lines = block_lines(
            &Block::new(
                BlockKind::Assistant,
                "Codex",
                "```powershell\ncargo run --release\n```",
            ),
            30,
        );
        let rendered = lines.iter().map(painted).collect::<Vec<_>>();

        assert_eq!(
            rendered[0],
            format!("● ┌─ powershell {}┐", "─".repeat(8))
        );
        assert_eq!(
            rendered[1],
            "  │ cargo run --release │"
        );
        assert_eq!(rendered[2], format!("  └{}┘", "─".repeat(21)));
        assert!(rendered[..3]
            .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) == 25));
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
            "변경: [src/main.rs](C:/Source/DevezCLI/src/main.rs:83), [Cargo.toml](C:/Source/DevezCLI/Cargo.toml:29)",
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
                composer_notice: Some("Copied to clipboard".to_owned()),
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
                    .any(|span| span.text == "Copied to clipboard")
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
                composer_notice: Some("Copied to clipboard".to_owned()),
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
                    .any(|span| span.text == "Copied to clipboard")
            })
            .expect("notice row");
        assert_eq!(
            painted(notice),
            format!("╰{}  Copied to clipboard ─╯", "─".repeat(54))
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
        let with_block = vec![Block::new(BlockKind::Assistant, "Codex", "done")];
        for live in [&[][..], &with_block[..]] {
            let bare = frame(live, None);
            let noticed = frame(live, Some("Copied to clipboard"));

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
        assert!(activity >= 2);
        assert!(frame.lines[activity - 2] == PaintLine::blank());
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
            Some("✓ Completed (1m 36s)"),
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
        assert!(activity >= 2);
        assert!(frame.lines[activity - 2] == PaintLine::blank());
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
        assert!(painted(&frame.lines[activity]).contains("Response: Short"));
        assert!(painted(&frame.lines[activity]).contains("Fast: Off"));
        assert!(!painted(&frame.lines[activity + 1]).contains("Response: Short"));
        assert_eq!(painted_width(&frame.lines[activity]), 158);
        assert_eq!(frame.lines[activity + 1].tone, Tone::EffortHigh);
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
        assert!(painted(&frame.lines[activity]).contains("Response: Short"));
        assert!(!painted(&frame.lines[activity]).contains("Shell: Collapse"));
        assert!(!painted(&frame.lines[activity + 1]).contains("Response: Short"));
    }

    #[test]
    fn working_activity_uses_its_model_tone() {
        let line = activity_lines("Working.. (2m 12s)", Some("gpt-5.6-terra"), 0.5, 80)
            .pop()
            .expect("working row");

        assert_eq!(line.prefix, " ");
        assert_eq!(line.text, "✽ ");
        assert_eq!(line.tone, Tone::ModelTerra);
        assert!(line.tail.iter().any(
            |span| span.text == " (2m 12s)" && span.tone == Tone::ModelTerra
        ));
        assert_eq!(
            line.tail
                .iter()
                .filter_map(|span| match span.tone {
                    Tone::Shimmer(_, _) => Some(span.text.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            "Working.."
        );
    }

    #[test]
    fn completed_activity_label_is_static() {
        let line = activity_lines("Completed (2m 12s)", Some("gpt-5.6-terra"), 0.5, 80)
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
    fn copy_notice_activity_uses_plain_text() {
        let line = activity_lines("Copied to clipboard", None, 0.5, 80)
            .pop()
            .expect("copy notice row");

        assert_eq!(line.tone, Tone::Plain);
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
            label: label.to_owned(),
            accent,
            response_length: "Short".to_owned(),
            fast_mode,
            effort: "high".to_owned(),
            cost: None,
            shell_display_mode: "Collapse".to_owned(),
            diff_display_mode: "Collapse".to_owned(),
            info_panel_open: false,
        }
    }

    #[test]
    fn composer_chrome_uses_effort_while_the_prompt_stays_plain() {
        let editor = Editor::default();
        let mode = test_mode("Default", ModeAccent::Calm, false);

        let (rows, _, _) = input_lines(&editor, &[], 80, "", "Ask anything", None, Some(&mode));

        assert_eq!(rows[0].tone, Tone::EffortHigh);
        assert_eq!(rows[1].prefix_tone, Tone::EffortHigh);
        assert_eq!(rows[1].tone, Tone::Plain);
        assert_eq!(
            rows[1].tail.last().map(|span| span.tone),
            Some(Tone::EffortHigh)
        );
        assert_eq!(rows.last().map(|line| line.tone), Some(Tone::EffortHigh));
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
        assert_eq!(
            texts,
            [
                "  ",
                "Response: Short",
                " · ",
                "Shell: Collapse",
                " · ",
                "Diff: Collapse",
                " · ",
                "Fast: On",
                " · ",
                "Panel: Closed",
                " ",
                "─╮"
            ]
        );
        assert_eq!(line.tail[1].tone, Tone::Muted);
        assert_eq!(line.tail[7].tone, Tone::FastOn);
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
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.info_panel_open = true;
        let line = input_top_line(120, "", Some(&mode));
        assert_eq!(pick_on(&line, "Response: Short"), Some(Pick::ResponseLength));
        assert_eq!(
            pick_on(&line, "Shell: Collapse"),
            Some(Pick::ShellDisplayMode)
        );
        assert_eq!(
            pick_on(&line, "Diff: Collapse"),
            Some(Pick::DiffDisplayMode)
        );
        assert_eq!(
            pick_on(&line, "Panel: Opened"),
            Some(Pick::InfoPanel)
        );
        assert_eq!(pick_on(&line, "Fast: On"), Some(Pick::FastMode));
        // The rule, and the middle of the separator between the badges, are not
        // settings — the columns beside each badge belong to that badge.
        assert_eq!(pick_mid(&line, " · "), None);
        assert_eq!(line.pick.as_ref().unwrap().at(0), None);
    }

    #[test]
    fn response_length_is_clickable_without_an_access_badge() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(80, "", Some(&mode));

        assert!(!painted(&line).contains("Full Access"));
        assert_eq!(pick_on(&line, "Response: Short"), Some(Pick::ResponseLength));
    }

    /// The cost pushes both badges right and a recalled-history label pushes the
    /// whole rule along with them, so the columns are only ever read off the
    /// spans as painted.
    #[test]
    fn the_cost_and_a_rule_label_do_not_move_the_badges_out_from_under_the_click() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());
        let line = input_top_line(120, "3/12", Some(&mode));

        assert_eq!(pick_on(&line, "Response: Short"), Some(Pick::ResponseLength));
        assert_eq!(pick_on(&line, "Fast: On"), Some(Pick::FastMode));
        assert_eq!(pick_on(&line, "[$0.95]"), None);
        assert_eq!(pick_on(&line, "3/12"), None);
    }

    /// A rule too narrow for the fast flag drops it; fixed access survives but
    /// never claims click columns.
    #[test]
    fn a_dropped_fast_flag_leaves_only_the_mode_clickable() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(24, "", Some(&mode));

        assert!(!painted(&line).contains("Fast"));
        assert_eq!(pick_on(&line, "Response: Short"), Some(Pick::ResponseLength));
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
        assert_eq!(
            texts,
            [
                "  ",
                "Response: Short",
                " · ",
                "Shell: Collapse",
                " · ",
                "Diff: Collapse",
                " ",
                "─╮"
            ]
        );
        assert_eq!(line.tail[3].tone, Tone::Muted);
    }

    #[test]
    fn estimated_cost_leads_the_badge() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());
        let line = input_top_line(80, "", Some(&mode));
        let texts = line
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rule_width(&line), 80);
        assert_eq!(
            texts,
            [
                "  ",
                "Response: Short",
                " · ",
                "Shell: Collapse",
                " · ",
                "Diff: Collapse",
                " ",
                "─╮"
            ]
        );
        assert_eq!(line.tail[1].tone, Tone::Muted);
        assert_eq!(line.tail[3].tone, Tone::Muted);
        assert_eq!(line.tail[5].tone, Tone::Muted);
    }

    #[test]
    fn shell_badge_follows_the_permission_mode() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());
        let line = input_top_line(80, "", Some(&mode));

        assert_eq!(
            line.tail
                .iter()
                .map(|span| span.text.as_str())
                .collect::<Vec<_>>(),
            [
                "  ",
                "Response: Short",
                " · ",
                "Shell: Collapse",
                " · ",
                "Diff: Collapse",
                " ",
                "─╮"
            ]
        );
        assert_eq!(
            pick_on(&line, "Shell: Collapse"),
            Some(Pick::ShellDisplayMode)
        );
        assert_eq!(
            pick_on(&line, "Diff: Collapse"),
            Some(Pick::DiffDisplayMode)
        );
    }

    /// The cost is the first thing to go: it is the least load-bearing segment.
    #[test]
    fn the_cost_is_dropped_before_the_fast_flag() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());
        let line = input_top_line(66, "", Some(&mode));
        let texts = line
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rule_width(&line), 66);
        assert_eq!(
            texts,
            [
                "  ",
                "Response: Short",
                " · ",
                "Shell: Collapse",
                " · ",
                "Diff: Collapse",
                " ",
                "─╮"
            ]
        );
    }

    #[test]
    fn tight_composer_rule_keeps_the_mode_and_drops_the_fast_flag() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(24, "", Some(&mode));
        let texts = line
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rule_width(&line), 24);
        assert_eq!(texts, ["  ", "Response: Short", " ", "─╮"]);
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

        assert_eq!(user_lines[1].prefix, " ");
        assert_eq!(user_lines[1].text, "hello");
        assert!(user_lines[1].prefix_tone == Tone::Plain);
        assert!(user_lines[1].tone == Tone::UserPrompt);
        assert!(user_lines[1].bold);
        assert_eq!(assistant_lines[0].prefix, "● ");
        assert_eq!(assistant_lines[0].prefix_tone, Tone::Accent);
        assert_eq!(assistant_lines[0].text, "hi");
        assert!(user_lines.iter().all(|line| line.text != "You"));
        assert!(assistant_lines.iter().all(|line| line.text != "Codex"));
    }

    #[test]
    fn every_circular_transcript_gutter_uses_the_theme_accent() {
        for block in [
            Block::new(BlockKind::Assistant, "Codex", "answer"),
            Block::new(BlockKind::Tool, "Shell", "output"),
            Block::new(BlockKind::FileChange, "Update(src/main.rs)", "Added 1 · Removed 0"),
            Block::new(BlockKind::Diff, "Diff", "changed"),
        ] {
            let line = block_lines(&block, 80)
                .into_iter()
                .find(|line| line.prefix == "● ")
                .expect("circular gutter");
            assert_eq!(line.prefix_tone, Tone::Accent);
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
                "done",
                ""
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
    fn sticky_prompt_uses_the_latest_prompt_wholly_above_the_viewport() {
        let anchors = vec![
            PromptAnchor::new(1..3, "first"),
            PromptAnchor::new(5..7, "second"),
        ];

        assert_eq!(
            sticky_prompt_for_viewport(&anchors, 7, 80).map(|line| painted(&line)),
            Some(" second".to_owned())
        );
    }

    #[test]
    fn sticky_prompt_is_hidden_while_its_source_rows_are_visible() {
        let anchors = vec![PromptAnchor::new(1..3, "first")];

        assert!(sticky_prompt_for_viewport(&anchors, 2, 80).is_none());
    }

    #[test]
    fn composed_screen_reserves_a_transcript_row_for_a_sticky_prompt() {
        let sticky = PaintLine {
            prefix: " ".to_owned(),
            prefix_tone: Tone::Plain,
            text: "first".to_owned(),
            tone: Tone::UserPrompt,
            bold: true,
            tool_heading: None,
            pick: None,
            tail: Vec::new(),
        };
        let (screen, cursor) = compose_screen(
            &text_rows(4, "row"),
            text_rows(1, "composer"),
            3,
            1,
            0,
            Some(sticky),
        );

        assert_eq!(
            screen.iter().map(painted).collect::<Vec<_>>(),
            [" first", "row1", "row2", "composer0"]
        );
        assert_eq!(cursor, 3);
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
                compose_screen(&transcript, live.clone(), view_rows, start, 1, None);

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
        };
        let (view_rows, live_rows) = split_rows(10, frame.lines.len(), 0);
        fit_frame(&mut frame, live_rows);
        let (screen, cursor_line) =
            compose_screen(&[], frame.lines, view_rows, 0, frame.cursor_line, None);

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
    fn fullscreen_scroll_to_bottom_returns_to_the_latest_transcript() {
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.scroll_back = 8;

        assert!(renderer.scroll_to_bottom());
        assert_eq!(renderer.scroll_back, 0);
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
        assert_eq!(word_background(control.tone), Some(theme::palette().hover_bg));
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
        assert_eq!(scroll_to_bottom_overlay_row(12, Some(7)), Some(15));

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
        assert_eq!(renderer.finish_selection(12, 0), SelectionResult::Click(12, 0));
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
    fn hide_omits_shell_and_web_search_blocks() {
        for hidden in [
            Block::new(BlockKind::Tool, "Running 1 shell command", ""),
            Block::new(BlockKind::Tool, "Running Shell Command", ""),
            Block::new(BlockKind::Warning, "Running 2 Shell Commands", ""),
            Block::new(BlockKind::System, "Running Shell Command", ""),
            Block::new(BlockKind::Tool, "Command", "Running Shell Command"),
            Block::new(BlockKind::Tool, "Web search", ""),
            Block::new(BlockKind::Tool, "Web search · rust ownership", ""),
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
        let range = |start: (u16, u16), end: (u16, u16)| CellRange {
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
        renderer.reconcile_selection(&[
            PaintLine::plain("selected"),
            PaintLine::plain("changed status"),
        ]);
        assert_eq!(
            renderer.finish_selection(2, 0),
            SelectionResult::Copy("sel".to_owned())
        );

        assert!(renderer.begin_selection(0, 0));
        assert!(renderer.update_selection(2, 0));
        renderer.reconcile_selection(&[PaintLine::plain("replaced"), PaintLine::plain("status")]);
        assert_eq!(renderer.finish_selection(2, 0), SelectionResult::None);
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
                "Updated Plan",
                "└ why\n✔ first\n▸ second\n□ third",
            ),
            80,
        );

        assert_eq!(lines[0].prefix, "• ");
        assert_eq!(lines[0].text, "Updated Plan");
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
        let lines = block_lines(&Block::new(BlockKind::Plan, "Updated Plan", "└ why"), 80);

        assert_eq!(lines[1].text, "why");
        assert_eq!(lines[2].text, "(no steps provided)");
    }

    #[test]
    fn status_line_is_trimmed_to_terminal_width() {
        let line = status_line_row(
            Some(StatusLineView {
                branch: Some("main".to_owned()),
                model: Some("GPT-5.6 Codex".to_owned()),
                effort: Some("xhigh".to_owned()),
                context: Some("ctx: 45k/256k (18%)".to_owned()),
                five_hour_percent: Some(12),
                weekly_percent: Some(34),
                reset_credits: None,
                notice: Some("connected".to_owned()),
            }),
            "",
            32,
        );
        assert!(painted_width(&line) <= 32);
        assert!(line.text.starts_with("main"));
        assert!(painted(&line).ends_with("..."));
    }

    #[test]
    fn status_line_marks_an_omitted_model_effort_hint_with_dots() {
        let line = status_line_row(
            Some(StatusLineView {
                branch: Some("main".to_owned()),
                model: Some("GPT-5.6 Sol".to_owned()),
                effort: Some("high".to_owned()),
                context: None,
                five_hour_percent: None,
                weekly_percent: None,
                reset_credits: None,
                notice: None,
            }),
            "",
            48,
        );

        assert!(painted(&line).ends_with("..."));
    }

    #[test]
    fn status_line_places_the_model_and_effort_shortcuts_at_the_far_right() {
        let line = status_line_row(
            Some(StatusLineView {
                branch: Some("main".to_owned()),
                model: Some("GPT-5.6 Codex".to_owned()),
                effort: Some("xhigh".to_owned()),
                context: Some("ctx: 45k/256k (18%)".to_owned()),
                five_hour_percent: Some(12),
                weekly_percent: Some(34),
                reset_credits: None,
                notice: None,
            }),
            "",
            120,
        );

        assert!(painted(&line).ends_with("Shift + ↑↓ model · ←→ effort"));
        assert_eq!(painted_width(&line), 118);
    }

    #[test]
    fn status_line_omits_a_disabled_branch_slot() {
        let line = status_line_row(
            Some(StatusLineView {
                branch: None,
                model: Some("GPT-5.6 Sol".to_owned()),
                effort: Some("high".to_owned()),
                context: None,
                five_hour_percent: None,
                weekly_percent: None,
                reset_credits: None,
                notice: None,
            }),
            "",
            80,
        );

        assert_eq!(line.prefix, " ");
        assert_eq!(line.text, "GPT-5.6 Sol");
    }

    /// The two readings the status line lets you change answer to a click; the
    /// ones that only report — branch, context, limits — do not.
    #[test]
    fn the_model_and_effort_readings_are_the_only_clickable_status_spans() {
        let line = status_line_row(
            Some(StatusLineView {
                branch: Some("main".to_owned()),
                model: Some("GPT-5.6 Sol".to_owned()),
                effort: Some("high".to_owned()),
                context: Some("ctx: 45k/256k (18%)".to_owned()),
                five_hour_percent: Some(12),
                weekly_percent: Some(34),
                reset_credits: Some("reset: 3 · 5d".to_owned()),
                notice: None,
            }),
            "",
            80,
        );

        assert_eq!(pick_on(&line, "GPT-5.6 Sol"), Some(Pick::Model));
        assert_eq!(pick_on(&line, "eff: high"), Some(Pick::EffortSetting));
        assert_eq!(pick_on(&line, "main"), None);
        assert_eq!(pick_on(&line, "ctx:"), None);
        assert_eq!(pick_on(&line, "5h: 12%"), None);
        assert_eq!(pick_on(&line, "week: 34%"), None);
        assert!(painted(&line).contains("reset: 3 · 5d"));
        assert_eq!(
            line.tail
                .iter()
                .find(|span| span.text == "reset: 3 · 5d")
                .map(|span| span.tone),
            Some(Tone::ResetCredit)
        );
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
            plan: "Pro Lite".to_owned(),
            credits: vec!["3 available".to_owned(), "· 2026-08-01  6d left".to_owned()],
            cwd: "C:/Source/DevezCLI".to_owned(),
            account: "dev@example.com".to_owned(),
        }
    }

    #[test]
    fn welcome_panel_fills_the_terminal_and_shows_the_version() {
        for width in [70u16, 90, 140] {
            let lines = welcome_lines(test_welcome(), width);
            let expected = panel_span(width);

            assert!(
                lines.iter().all(|line| painted_width(line) == expected),
                "width {width}: rows are not all {expected} columns"
            );
            assert!(
                lines
                    .iter()
                    .any(|line| painted(line)
                        .contains(&format!("v{}", crate::update::CURRENT_VERSION))),
                "width {width}: version missing from the headline"
            );
        }
    }

    #[test]
    fn wide_welcome_panel_reserves_a_notes_column() {
        let lines = welcome_lines(test_welcome(), 110);

        assert!(painted(&lines[0]).contains('┬'));
        assert!(painted(lines.last().expect("bottom border")).contains('┴'));
        assert!(
            lines
                .iter()
                .any(|line| painted(line).contains("What's new"))
        );
        // Every body row carries the divider, so the column never collapses.
        assert!(
            lines[1..lines.len() - 1].iter().all(|line| line
                .tail
                .iter()
                .filter(|span| span.text == "│")
                .count()
                == 2)
        );
    }

    #[test]
    fn wide_welcome_panel_keeps_info_column_at_48_cells() {
        let lines = welcome_lines(test_welcome(), 110);
        let top = painted(&lines[0]);
        let (left, right) = top
            .trim_matches(['╭', '╮'])
            .split_once('┬')
            .expect("split border");

        assert_eq!(left.chars().count(), 48);
        assert_eq!(right.chars().count(), panel_span(110) - 2 - 48 - 1);
    }

    #[test]
    fn narrow_welcome_panel_collapses_to_one_column() {
        let lines = welcome_lines(test_welcome(), 50);

        assert!(!painted(&lines[0]).contains('┬'));
        assert!(
            lines
                .iter()
                .all(|line| !painted(line).contains("What's new"))
        );
        assert!(
            lines
                .iter()
                .all(|line| painted_width(line) == panel_span(50))
        );
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
            plan: "Pro".to_owned(),
            cwd: r"C:\Source\DevezCLI".to_owned(),
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

        assert!(painted.contains("DEVEZ CLI"), "{painted}");
        assert!(painted.contains("someone@example.com"), "{painted}");
        // The card sits above the picker rather than replacing it.
        assert!(frame.lines[0].text.starts_with('╭'));
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
            while let Some(ch) = chars.next() {
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
                branch: Some("main".to_owned()),
                model: Some("GPT-5.6 Sol".to_owned()),
                effort: Some("high".to_owned()),
                context: None,
                five_hour_percent: None,
                weekly_percent: Some(34),
                reset_credits: None,
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
        let start = UnicodeWidthStr::width(&text[..text.find("Response: Short").unwrap()]);

        // A column either side of the response label is part of the target.
        assert_eq!(
            Renderer::hover_columns(&line, None, Some(&Pick::ResponseLength)),
            Some(start - 1..start + 16)
        );
        // Nothing on the rule answers for a pick it does not carry.
        assert_eq!(
            Renderer::hover_columns(&line, None, Some(&Pick::Model)),
            None
        );
        assert_eq!(Renderer::hover_columns(&line, None, None), None);
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
        let response = Renderer::hover_columns(&line, None, Some(&Pick::ResponseLength))
            .expect("response badge");
        let fast = Renderer::hover_columns(&line, None, Some(&Pick::FastMode)).expect("fast badge");
        let mut output = Vec::new();

        for columns in hover_repaint_columns(Some(response), Some(fast.clone())) {
            print_line_columns(&mut output, &line, None, Some(fast.clone()), columns)
                .expect("partial repaint");
        }

        let painted = String::from_utf8(output).expect("utf-8 paint");
        assert!(painted.contains("Response: Short"));
        assert!(painted.contains("Fast: On"));
        assert!(!painted.contains("Shell: Collapse"));
        assert!(!painted.contains("Diff: Collapse"));
        assert!(!painted.contains("Panel: Closed"));
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
            model_tone("GPT-5.6-Sol"),
            model_tone("GPT-5.6-Terra"),
            model_tone("GPT-5.6-Luna"),
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
    fn terra_uses_the_model_family_pink() {
        assert_eq!(tone_rgb(Tone::ModelTerra), Some(theme::palette().pink));
    }

    #[test]
    fn reset_credit_uses_the_theme_orange() {
        assert_eq!(tone_rgb(Tone::ResetCredit), Some(theme::palette().orange));
    }

    #[test]
    fn panel_borders_use_the_theme_border_tone() {
        let lines = welcome_lines(
            WelcomeView {
                plan: "Pro".to_owned(),
                credits: vec!["none available".to_owned()],
                cwd: "C:\\work".to_owned(),
                account: "ChatGPT".to_owned(),
            },
            80,
        );

        assert!(lines.first().is_some_and(|line| line.tone == Tone::Border));
        assert!(lines.last().is_some_and(|line| line.tone == Tone::Border));
        assert!(lines[1].prefix_tone == Tone::Border);
        assert!(
            lines[1]
                .tail
                .first()
                .is_some_and(|span| span.tone == Tone::Border)
        );
    }
}
