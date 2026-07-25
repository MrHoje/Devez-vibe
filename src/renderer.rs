use std::io::{Stdout, Write, stdout};

use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveDown, MoveTo, MoveToColumn, MoveUp, Show, position as cursor_position},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size as terminal_size},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    editor::Editor,
    theme::{self, Rgb, ThemeKind},
};

#[derive(Clone, Copy)]
pub enum BlockKind {
    Welcome,
    Update,
    User,
    Assistant,
    Reasoning,
    Tool,
    Diff,
    ModelChange,
    Warning,
    Error,
    System,
}

#[derive(Clone)]
pub struct Block {
    pub kind: BlockKind,
    pub title: String,
    pub body: String,
}

impl Block {
    pub fn new(kind: BlockKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            body: body.into(),
        }
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
    pub hint: String,
    pub style: OverlayStyle,
    pub input: Option<&'a Editor>,
    pub input_label: &'static str,
    pub input_placeholder: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OverlayStyle {
    Panel,
    Picker,
}

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
}

pub struct StatusLineView {
    pub branch: Option<String>,
    pub model: String,
    pub effort: String,
    pub context: Option<String>,
    pub five_hour_percent: Option<u8>,
    pub weekly_percent: Option<u8>,
    pub notice: Option<String>,
}

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
    pub fast_mode: bool,
    /// What the thread is estimated to have cost so far. Absent before the first
    /// turn reports usage, and whenever the model has no published rate.
    pub cost: Option<String>,
}

pub struct View<'a> {
    pub live_blocks: Vec<Block>,
    pub overlay: Option<OverlayView<'a>>,
    pub editor: &'a Editor,
    pub welcome: Option<WelcomeView>,
    pub suggestions: Vec<SuggestionView>,
    pub activity: Option<String>,
    pub footer: String,
    pub status_line: Option<StatusLineView>,
    pub composer_notice: Option<String>,
    pub composer_mode: Option<ComposerMode>,
}

pub struct TerminalSession;

impl TerminalSession {
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(stdout(), EnableBracketedPaste, Show)?;
        Ok(Self)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
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
    previous_lines: Vec<PaintLine>,
    cursor_line: usize,
    last_width: u16,
    last_height: u16,
    theme: ThemeKind,
    history: Vec<Block>,
}

impl Renderer {
    pub fn new(selected_theme: ThemeKind) -> Self {
        theme::set_current(selected_theme);
        Self {
            out: stdout(),
            previous_lines: Vec::new(),
            cursor_line: 0,
            last_width: 0,
            last_height: 0,
            theme: selected_theme,
            history: Vec::new(),
        }
    }

    pub fn clear_screen(&mut self) -> Result<()> {
        self.history.clear();
        self.apply_terminal_theme()?;
        self.reset_screen()
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
        self.reset_screen()?;
        let width = terminal_size().unwrap_or((100, 30)).0.max(20);
        // Moved out so the blocks can be printed without cloning the transcript;
        // put back whether or not a write fails partway.
        let history = std::mem::take(&mut self.history);
        let mut outcome = Ok(());
        for block in &history {
            let lines = block_lines(block, width);
            if let Err(error) = self.print_permanent(block, &lines) {
                outcome = Err(error);
                break;
            }
        }
        self.history = history;
        outcome
    }

    fn reset_screen(&mut self) -> Result<()> {
        self.previous_lines.clear();
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
        let (width, height) = terminal_size().unwrap_or((100, 30));
        let status = StatusArea {
            fallback: view.footer,
            line: view.status_line,
            composer_notice: view.composer_notice,
            composer_mode: view.composer_mode,
        };
        let mut frame = if let Some(overlay) = view.overlay {
            overlay_frame(&view.live_blocks, overlay, status, width.max(20))
        } else {
            normal_frame(
                &view.live_blocks,
                view.editor,
                view.welcome,
                &view.suggestions,
                view.activity.as_deref(),
                status,
                width.max(20),
            )
        };

        let max_live = height.max(3) as usize;
        let natural_rows = frame.lines.len().min(max_live);
        let needs_full_repaint = self.previous_lines.is_empty()
            || self.last_width != width
            || self.last_height != height
            || !committed.is_empty();
        if needs_full_repaint {
            self.erase_live()?;
            for block in committed {
                let lines = block_lines(block, width.max(20));
                self.print_permanent(block, &lines)?;
            }
            self.history.extend(committed.iter().cloned());
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

    pub fn finish(&mut self) -> Result<()> {
        self.erase_live()?;
        queue!(self.out, Show, ResetColor, Print("\r\n"))?;
        self.out.flush()?;
        Ok(())
    }

    fn erase_live(&mut self) -> Result<()> {
        if self.previous_lines.is_empty() {
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
        for (index, line) in lines.iter().enumerate() {
            if tagged {
                let marker_skip = usize::from(index == 0 && is_copy_marker(&line.prefix))
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
    Accent,
    User,
    Success,
    Warning,
    Error,
    Code,
    EffortLow,
    EffortMedium,
    EffortHigh,
    EffortXHigh,
    EffortMax,
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
    DiffHeader,
    CopyJoin,
}

#[derive(Clone, PartialEq, Eq)]
struct PaintLine {
    prefix: String,
    prefix_tone: Tone,
    text: String,
    tone: Tone,
    bold: bool,
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
            tail: Vec::new(),
        }
    }

    fn blank() -> Self {
        Self::plain("")
    }
}

fn normal_frame(
    live: &[Block],
    editor: &Editor,
    welcome: Option<WelcomeView>,
    suggestions: &[SuggestionView],
    activity: Option<&str>,
    status: StatusArea,
    width: u16,
) -> Frame {
    let mut lines = Vec::new();
    if let Some(welcome) = welcome {
        lines.extend(welcome_lines(welcome, width));
        lines.push(PaintLine::blank());
    }

    for block in live {
        lines.extend(block_lines(block, width));
    }
    if !live.is_empty() {
        lines.push(PaintLine::blank());
    }

    let dock_index = lines.len();
    if !suggestions.is_empty() {
        lines.extend(suggestion_lines(suggestions, width));
    }

    // Transient rows ride directly above the composer rule so they stay pinned to
    // the prompt instead of scrolling away with the conversation.
    if let Some(activity) = activity {
        lines.extend(wrapped_line(
            "",
            Tone::Accent,
            activity,
            Tone::Accent,
            false,
            width,
        ));
    }
    if let Some(notice) = status.composer_notice.as_deref() {
        lines.extend(wrapped_line(
            "",
            Tone::Success,
            notice,
            Tone::Success,
            false,
            width,
        ));
    }
    // One blank row keeps a transient message off the composer rule; without a
    // transient row the composer stays flush against whatever is above it.
    if activity.is_some() || status.composer_notice.is_some() {
        lines.push(PaintLine::blank());
    }

    let (input_lines, input_cursor_line, input_cursor_col) =
        input_lines(editor, width, "", "", status.composer_mode.as_ref());
    let cursor_line = lines.len() + input_cursor_line;
    lines.extend(input_lines);
    lines.push(status_line_row(status.line, &status.fallback, width));

    Frame {
        lines,
        cursor_line,
        cursor_col: input_cursor_col,
        show_cursor: true,
        dock_index,
    }
}

/// Narrowest inner width that still leaves both columns readable; below this the
/// welcome panel collapses to a single column.
const WELCOME_SPLIT_MIN: usize = 62;

fn welcome_lines(welcome: WelcomeView, width: u16) -> Vec<PaintLine> {
    let panel_width = panel_span(width);
    let inner_width = panel_width.saturating_sub(2);
    let left = welcome_info_rows(&welcome, inner_width);

    if inner_width < WELCOME_SPLIT_MIN {
        let mut lines = vec![panel_top(inner_width), panel_padding_row(panel_width)];
        lines.extend(
            left.into_iter()
                .map(|(text, tone, bold)| panel_line(&text, panel_width, tone, bold)),
        );
        lines.push(panel_padding_row(panel_width));
        lines.push(panel_bottom(inner_width));
        return lines;
    }

    // The right column is reserved for release notes, so give it a fixed slice
    // and let the info column absorb the rest of the terminal.
    let right_width = (inner_width * 2 / 5).clamp(26, 52);
    let left_width = inner_width - right_width - 1;
    let left = welcome_info_rows(&welcome, left_width);
    let right = welcome_notes_rows(right_width);

    let mut lines = vec![PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: format!("╭{}┬{}╮", "─".repeat(left_width), "─".repeat(right_width)),
        tone: Tone::Border,
        bold: false,
        tail: Vec::new(),
    }];
    lines.push(split_panel_line(None, left_width, None, right_width));
    for row in 0..left.len().max(right.len()) {
        lines.push(split_panel_line(
            left.get(row),
            left_width,
            right.get(row),
            right_width,
        ));
    }
    lines.push(split_panel_line(None, left_width, None, right_width));
    lines.push(PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: format!("╰{}┴{}╯", "─".repeat(left_width), "─".repeat(right_width)),
        tone: Tone::Border,
        bold: false,
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
        (
            "  /help commands  ·  /model switch model".to_owned(),
            Tone::Muted,
            false,
        ),
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
fn panel_rule_row(opening: &str, label: &str, corner: char, panel_width: usize) -> PaintLine {
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
        tail: vec![PaintSpan {
            text: format!("{}{corner}", "─".repeat(panel_width.saturating_sub(used))),
            tone: Tone::Border,
            bold: false,
        }],
    }
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

fn panel_top(inner_width: usize) -> PaintLine {
    PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: format!("╭{}╮", "─".repeat(inner_width)),
        tone: Tone::Border,
        bold: false,
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
        tail: Vec::new(),
    }
}

/// Full-width update banner: a rule, the headline, the hint, and a closing rule.
fn update_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let rule_width = (width as usize).max(1);
    let text_width = rule_width.saturating_sub(1);
    let rule = || PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: "─".repeat(rule_width),
        tone: Tone::Border,
        bold: false,
        tail: Vec::new(),
    };
    vec![
        rule(),
        PaintLine {
            prefix: " ".to_owned(),
            prefix_tone: Tone::Accent,
            text: compact_text(&block.title, text_width),
            tone: Tone::Accent,
            bold: true,
            tail: Vec::new(),
        },
        PaintLine {
            prefix: " ".to_owned(),
            prefix_tone: Tone::Muted,
            text: compact_text(&block.body, text_width),
            tone: Tone::Muted,
            bold: false,
            tail: Vec::new(),
        },
        rule(),
        PaintLine::blank(),
    ]
}

fn suggestion_lines(suggestions: &[SuggestionView], width: u16) -> Vec<PaintLine> {
    let panel_width = panel_span(width);
    let inner_width = panel_width.saturating_sub(2);
    const HEADER: &str = "Commands ";
    // "╭─ " + header + rule + "╮" has to land on exactly panel_width columns.
    let header_rule = panel_width
        .saturating_sub(3 + UnicodeWidthStr::width(HEADER) + 1)
        .max(1);
    let mut lines = vec![PaintLine {
        prefix: "╭─ ".to_owned(),
        prefix_tone: Tone::Border,
        text: HEADER.to_owned(),
        tone: Tone::Muted,
        bold: false,
        tail: vec![PaintSpan {
            text: format!("{}╮", "─".repeat(header_rule)),
            tone: Tone::Border,
            bold: false,
        }],
    }];
    lines.push(panel_padding_row(panel_width));
    for suggestion in suggestions.iter().take(6) {
        let marker = if suggestion.selected { "❯" } else { " " };
        let content = format!(
            " {marker} {:<10} {}",
            suggestion.command, suggestion.description
        );
        lines.push(panel_line(
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
    lines.push(panel_padding_row(panel_width));
    lines.push(panel_bottom(inner_width));
    lines
}

/// A bordered blank row. Every boxed list gets one under its top rule and one
/// above its bottom rule, so the contents never sit flush against the border.
fn panel_padding_row(panel_width: usize) -> PaintLine {
    panel_line("", panel_width, Tone::Muted, false)
}

fn panel_line(text: &str, width: usize, tone: Tone, bold: bool) -> PaintLine {
    let inner_width = width.saturating_sub(2);
    let content = compact_text(text, inner_width);
    let padding = inner_width.saturating_sub(UnicodeWidthStr::width(content.as_str()));
    PaintLine {
        prefix: "│".to_owned(),
        prefix_tone: Tone::Border,
        text: format!("{content}{}", " ".repeat(padding)),
        tone,
        bold,
        tail: vec![PaintSpan {
            text: "│".to_owned(),
            tone: Tone::Border,
            bold: false,
        }],
    }
}

fn overlay_frame(
    live: &[Block],
    overlay: OverlayView<'_>,
    status: StatusArea,
    width: u16,
) -> Frame {
    let mut lines = Vec::new();
    for block in live {
        lines.extend(block_lines(block, width));
    }
    if !live.is_empty() {
        lines.push(PaintLine::blank());
    }
    let dock_index = lines.len();

    match overlay.style {
        OverlayStyle::Picker => {
            lines.push(PaintLine {
                prefix: "  ".to_owned(),
                prefix_tone: Tone::Accent,
                text: overlay.title,
                tone: Tone::Accent,
                bold: true,
                tail: Vec::new(),
            });
            lines.push(PaintLine::blank());
            for row in overlay.lines {
                if row.text.is_empty() {
                    lines.push(PaintLine::blank());
                    continue;
                }
                for (part_index, part) in row.text.lines().enumerate() {
                    let prefix = if part_index == 0 {
                        if row.selected { "  ❯ " } else { "    " }
                    } else {
                        "    "
                    };
                    lines.extend(wrapped_line(
                        prefix,
                        if row.selected {
                            Tone::Accent
                        } else {
                            Tone::Muted
                        },
                        part,
                        if row.muted {
                            Tone::Muted
                        } else if part.contains('●') && part.contains('○') {
                            Tone::Accent
                        } else {
                            model_tone(part).unwrap_or(Tone::Plain)
                        },
                        row.selected && part_index == 0,
                        width,
                    ));
                }
            }
            lines.push(PaintLine::blank());
            lines.push(PaintLine {
                prefix: "  ".to_owned(),
                prefix_tone: Tone::Muted,
                text: overlay.hint,
                tone: Tone::Muted,
                bold: false,
                tail: Vec::new(),
            });
        }
        OverlayStyle::Panel => {
            // A closed box: every row lands on exactly `panel_width` columns.
            let panel_width = panel_span(width);
            lines.push(panel_rule_row("╭─ ", &overlay.title, '╮', panel_width));
            lines.push(panel_padding_row(panel_width));
            for row in overlay.lines {
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
                    lines.extend(
                        wrapped
                            .into_iter()
                            .map(|line| close_panel_row(line, panel_width)),
                    );
                }
            }
            lines.push(panel_padding_row(panel_width));
            lines.push(panel_rule_row("╰─ ", &overlay.hint, '╯', panel_width));
        }
    }
    let mut cursor_line = lines.len() - 1;
    let mut cursor_col = 0;
    let show_cursor = if let Some(editor) = overlay.input {
        // The composer rule reads as part of the picker without this gap.
        lines.push(PaintLine::blank());
        let (input, input_cursor_line, input_cursor_col) = input_lines(
            editor,
            width,
            overlay.input_label,
            overlay.input_placeholder,
            status.composer_mode.as_ref(),
        );
        cursor_line = lines.len() + input_cursor_line;
        cursor_col = input_cursor_col;
        lines.extend(input);
        true
    } else {
        false
    };
    lines.push(status_line_row(status.line, &status.fallback, width));

    Frame {
        cursor_line,
        cursor_col,
        lines,
        show_cursor,
        dock_index,
    }
}

fn fit_frame(frame: &mut Frame, target_rows: usize) {
    let target_rows = target_rows.max(1);
    if frame.lines.len() > target_rows {
        let dropped = frame.lines.len() - target_rows;
        frame.lines.drain(0..dropped);
        frame.cursor_line = frame.cursor_line.saturating_sub(dropped);
        frame.dock_index = frame.dock_index.saturating_sub(dropped);
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
            tail: Vec::new(),
        };
    };

    let effort_tone = match status.effort.as_str() {
        "low" => Tone::EffortLow,
        "medium" => Tone::EffortMedium,
        "high" => Tone::EffortHigh,
        "xhigh" => Tone::EffortXHigh,
        "max" => Tone::EffortMax,
        _ => Tone::StatusText,
    };
    let mut spans = Vec::new();
    let branch = status
        .branch
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "--".to_owned());
    push_status_span(&mut spans, compact_right(&branch, 24), Tone::Branch);
    push_status_span(
        &mut spans,
        compact_right(&status.model, 28),
        model_tone(&status.model).unwrap_or(Tone::StatusText),
    );
    push_status_span(&mut spans, format!("eff: {}", status.effort), effort_tone);
    if let Some(context) = status.context.filter(|context| !context.is_empty()) {
        push_status_span(&mut spans, context, Tone::Context);
    }
    // The 5h window is dropped entirely when unknown rather than shown as a stub.
    if let Some(percent) = status.five_hour_percent {
        push_status_span(&mut spans, format!("5h: {percent}%"), Tone::LimitFiveHour);
    }
    push_status_span(
        &mut spans,
        status.weekly_percent.map_or_else(
            || "week: --".to_owned(),
            |percent| format!("week: {percent}%"),
        ),
        Tone::LimitWeekly,
    );
    // Fast On/Off lives on the composer top rule beside the permission mode.
    if let Some(notice) = status.notice.filter(|notice| !notice.is_empty()) {
        push_status_span(&mut spans, notice, Tone::Muted);
    }
    trim_spans(&mut spans, width.max(1) as usize);

    let first = spans.first().cloned().unwrap_or(PaintSpan {
        text: String::new(),
        tone: Tone::Muted,
        bold: false,
    });
    PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Border,
        text: first.text,
        tone: first.tone,
        bold: first.bold,
        tail: spans.into_iter().skip(1).collect(),
    }
}

fn push_status_span(spans: &mut Vec<PaintSpan>, text: impl Into<String>, tone: Tone) {
    if spans.is_empty() {
        spans.push(PaintSpan {
            text: text.into(),
            tone,
            bold: false,
        });
        return;
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
}

fn trim_spans(spans: &mut Vec<PaintSpan>, max_width: usize) {
    let mut overflow = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum::<usize>()
        .saturating_sub(max_width);
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
}

/// Rows of tool output kept on screen, matching Claude Code's cap. Whatever is
/// left over is reported as a count instead of being printed.
const TOOL_OUTPUT_ROWS: usize = 5;

/// Command and tool results: the heading, then at most [`TOOL_OUTPUT_ROWS`] rows
/// of raw output and a muted `… +N lines`. Output is printed verbatim rather
/// than through the markdown pipeline — a shell writes text, not documents.
fn tool_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let mut lines = wrapped_line("● ", Tone::User, &block.title, Tone::Plain, true, width);
    // Trailing blank rows are noise, and a shell almost always leaves one.
    let rows = block
        .body
        .trim_end_matches(['\n', '\r'])
        .lines()
        .collect::<Vec<_>>();
    if block.body.is_empty() || rows.is_empty() {
        return lines;
    }
    let shown = rows.len().min(TOOL_OUTPUT_ROWS);
    for row in &rows[..shown] {
        lines.extend(wrapped_line(
            "  ",
            Tone::Muted,
            row,
            Tone::Muted,
            false,
            width,
        ));
    }
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

/// Title the app-server's reasoning summaries stream under, and the only one
/// that renders as a bare thought instead of a labelled section.
const THINKING_TITLE: &str = "Thinking…";

/// Reasoning summaries, shaped like Claude Code's: a narrow `∴` gutter and a
/// single dim italic paragraph. The summary's own line breaks are folded away
/// so a long thought stays a handful of wrapped rows rather than a document
/// that pushes the rest of the turn off screen. Markdown is left as written —
/// there are no headings or bullets to render inside one paragraph.
fn reasoning_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let body = block.body.split_whitespace().collect::<Vec<_>>().join(" ");
    // `/plan` output shares this block kind but keeps its heading.
    let titled = block.title != THINKING_TITLE;
    let mut lines = if titled {
        wrapped_line("✻ ", Tone::Muted, &block.title, Tone::Plain, true, width)
    } else {
        Vec::new()
    };
    if body.is_empty() {
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
    let (prefix, prefix_tone) = if titled {
        ("  ", Tone::Muted)
    } else {
        ("∴ ", Tone::Thinking)
    };
    lines.extend(wrapped_line(
        prefix,
        prefix_tone,
        &body,
        Tone::Thinking,
        false,
        width,
    ));
    lines
}

fn block_lines(block: &Block, width: u16) -> Vec<PaintLine> {
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
    if matches!(block.kind, BlockKind::Tool) {
        return tool_lines(block, width);
    }
    if matches!(block.kind, BlockKind::ModelChange) {
        return vec![
            PaintLine {
                prefix: "  ".to_owned(),
                prefix_tone: Tone::ModelChange,
                text: block.title.clone(),
                tone: Tone::ModelChange,
                bold: true,
                tail: Vec::new(),
            },
            PaintLine {
                prefix: "    ".to_owned(),
                prefix_tone: Tone::ModelChange,
                text: block.body.clone(),
                tone: Tone::ModelChange,
                bold: false,
                tail: Vec::new(),
            },
            PaintLine::blank(),
        ];
    }

    let (marker, tone) = match block.kind {
        BlockKind::Welcome | BlockKind::Update | BlockKind::ModelChange => {
            unreachable!("handled above")
        }
        BlockKind::User => unreachable!("user blocks are rendered separately"),
        BlockKind::Reasoning | BlockKind::Tool => unreachable!("handled above"),
        BlockKind::Assistant => ("● ", Tone::Plain),
        BlockKind::Diff => ("● ", Tone::Plain),
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
    for raw_line in block.body.lines() {
        let trimmed = raw_line.trim_start();
        if let Some(language) = trimmed.strip_prefix("```") {
            let text = if code {
                "└────────────────".to_owned()
            } else {
                let label = if language.trim().is_empty() {
                    "code"
                } else {
                    language.trim()
                };
                format!("┌─ {label}")
            };
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  ", Tone::Muted);
            lines.push(PaintLine {
                prefix,
                prefix_tone,
                text,
                tone: Tone::Muted,
                bold: false,
                tail: Vec::new(),
            });
            if code {
                code_language.clear();
            } else {
                code_language = language.trim().to_ascii_lowercase();
            }
            code = !code;
            continue;
        }

        if code {
            let (prefix, prefix_tone) =
                body_prefix(&mut first_content, marker, tone, "  │ ", Tone::Muted);
            lines.extend(code_line(
                &prefix,
                prefix_tone,
                raw_line,
                &code_language,
                width,
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
            prefix: "  ".to_owned(),
            prefix_tone: Tone::Muted,
            text: "└────────────────".to_owned(),
            tone: Tone::Muted,
            bold: false,
            tail: Vec::new(),
        });
    }
    lines.push(PaintLine::blank());
    lines
}

fn user_prompt_lines(block: &Block, width: u16) -> Vec<PaintLine> {
    let mut lines = Vec::new();
    if block.body.is_empty() {
        lines.extend(wrapped_line(
            "❯ ",
            Tone::Plain,
            "",
            Tone::UserPrompt,
            true,
            width,
        ));
        return lines;
    }

    for (index, raw_line) in block.body.lines().enumerate() {
        lines.extend(wrapped_line(
            if index == 0 { "❯ " } else { "  " },
            if index == 0 {
                Tone::Plain
            } else {
                Tone::Muted
            },
            raw_line,
            Tone::UserPrompt,
            true,
            width,
        ));
    }
    lines.push(PaintLine::blank());
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

fn code_line(
    prefix: &str,
    prefix_tone: Tone,
    text: &str,
    language: &str,
    width: u16,
) -> Vec<PaintLine> {
    if matches!(language, "diff" | "patch") {
        return diff_line(prefix, prefix_tone, text, width);
    }

    styled_lines(
        prefix,
        prefix_tone,
        highlight_code(text, language),
        Tone::Code,
        false,
        width,
    )
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
const COPY_MARKERS: [&str; 5] = ["● ", "✻ ", "▲ ", "✕ ", "◆ "];

fn is_copy_marker(prefix: &str) -> bool {
    COPY_MARKERS.contains(&prefix)
}

/// Blocks whose lines carry `devez-copy-v1` metadata. Excludes the ones drawn as
/// cards, whose box art is content the reader asked to see.
fn copy_metadata_applies(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::Assistant
            | BlockKind::Reasoning
            | BlockKind::Tool
            | BlockKind::Diff
            | BlockKind::Warning
            | BlockKind::Error
            | BlockKind::System
    )
}

fn input_lines(
    editor: &Editor,
    width: u16,
    label: &str,
    placeholder: &str,
    mode: Option<&ComposerMode>,
) -> (Vec<PaintLine>, usize, usize) {
    let panel_width = (width as usize).saturating_sub(1).max(16);
    let first_prefix = "> ";
    let continuation_prefix = "  ";
    let content_width = panel_width
        .saturating_sub(UnicodeWidthStr::width(first_prefix))
        .max(4);
    let mut raw_rows = vec![String::new()];
    let mut row = 0;
    let mut column = UnicodeWidthStr::width(first_prefix);
    let mut cursor_row = 0;
    let mut cursor_column = column;

    for (index, ch) in editor.chars().iter().copied().enumerate() {
        if index == editor.cursor() {
            cursor_row = row;
            cursor_column = column;
        }

        if ch == '\n' {
            raw_rows.push(String::new());
            row += 1;
            column = UnicodeWidthStr::width(continuation_prefix);
            continue;
        }

        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        let content_column = column.saturating_sub(UnicodeWidthStr::width(if row == 0 {
            first_prefix
        } else {
            continuation_prefix
        }));
        if content_column + ch_width > content_width && !raw_rows[row].is_empty() {
            raw_rows.push(String::new());
            row += 1;
            column = UnicodeWidthStr::width(continuation_prefix);
            if index == editor.cursor() {
                cursor_row = row;
                cursor_column = column;
            }
        }
        raw_rows[row].push(ch);
        column += ch_width;
    }

    if editor.cursor() == editor.chars().len() {
        cursor_row = row;
        cursor_column = column;
    }

    let mut rows = Vec::with_capacity(raw_rows.len() + 2);
    rows.push(input_top_line(panel_width, label, mode));
    for (index, raw) in raw_rows.into_iter().enumerate() {
        let is_placeholder = editor.is_empty() && index == 0;
        let content = if is_placeholder {
            placeholder.to_owned()
        } else {
            raw
        };
        rows.push(PaintLine {
            prefix: if index == 0 {
                first_prefix.to_owned()
            } else {
                continuation_prefix.to_owned()
            },
            // Prompt marker and cursor stay in the terminal's own colour.
            prefix_tone: Tone::Plain,
            text: content,
            tone: if is_placeholder {
                Tone::Muted
            } else {
                Tone::Plain
            },
            bold: false,
            tail: Vec::new(),
        });
    }
    rows.push(PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Muted,
        text: "─".repeat(panel_width),
        tone: Tone::Muted,
        bold: false,
        tail: Vec::new(),
    });

    (rows, cursor_row + 1, cursor_column)
}

/// Shortest rule stub kept on the composer top line so the frame never collapses.
const COMPOSER_RULE_MIN: usize = 4;
/// Blank columns between the rule and the mode badge.
const COMPOSER_MODE_GAP: usize = 2;
/// Rule segment trailing the mode badge, so the line reads as unbroken.
const COMPOSER_MODE_TAIL_RULE: usize = 2;
/// Separator between the permission mode and the fast-tier flag.
const COMPOSER_BADGE_SEPARATOR: &str = " · ";

fn input_top_line(panel_width: usize, label: &str, mode: Option<&ComposerMode>) -> PaintLine {
    let left = if label.is_empty() {
        String::new()
    } else {
        format!("── {label} ")
    };
    let left_width = UnicodeWidthStr::width(left.as_str());
    // Right-hand badges eat into this budget; whatever survives stays as rule.
    let mut budget = panel_width.saturating_sub(left_width + COMPOSER_RULE_MIN);

    // The mode is persistent state, so it anchors the far right.
    let badge = mode.and_then(|mode| {
        // Blanks either side of the badge plus the rule stub that trails it.
        let reserved = COMPOSER_MODE_GAP + 1 + COMPOSER_MODE_TAIL_RULE;
        let spans = fitting_badge_spans(mode, budget.saturating_sub(reserved))?;
        let width = spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.text.as_str()))
            .sum::<usize>();
        budget -= width + reserved;
        Some(spans)
    });

    let mut tail = Vec::new();
    if let Some(spans) = badge {
        tail.push(rule_gap(COMPOSER_MODE_GAP));
        tail.extend(spans);
        tail.push(rule_gap(1));
        tail.push(PaintSpan {
            text: "─".repeat(COMPOSER_MODE_TAIL_RULE),
            tone: Tone::Muted,
            bold: false,
        });
    }

    let fill = (COMPOSER_RULE_MIN + budget).min(panel_width.saturating_sub(left_width));
    PaintLine {
        prefix: String::new(),
        prefix_tone: Tone::Muted,
        text: format!("{left}{}", "─".repeat(fill)),
        tone: Tone::Muted,
        bold: false,
        tail,
    }
}

/// Widest badge that fits in `budget`, dropping segments from the right until it
/// does: mode · fast flag · estimated cost. The parts are never ellipsized — a
/// half-written mode name or a clipped price is worse than none.
fn fitting_badge_spans(mode: &ComposerMode, budget: usize) -> Option<Vec<PaintSpan>> {
    let mode_span = PaintSpan {
        text: mode.label.clone(),
        tone: mode_accent_tone(mode.accent),
        bold: false,
    };

    let fast_label = if mode.fast_mode {
        "Fast On"
    } else {
        "Fast Off"
    };
    let fast_spans = [
        separator_span(),
        PaintSpan {
            text: fast_label.to_owned(),
            tone: if mode.fast_mode {
                Tone::FastOn
            } else {
                Tone::FastOff
            },
            bold: false,
        },
    ];

    let cost_spans = mode.cost.as_deref().map(|cost| {
        vec![
            separator_span(),
            PaintSpan {
                text: cost.to_owned(),
                tone: Tone::Plain,
                bold: false,
            },
        ]
    });

    // Assemble every segment, then note where each rung of the ladder ends so
    // the badge can be peeled back from the right until it fits.
    let mut spans = vec![mode_span];
    let through_mode = spans.len();
    spans.extend(fast_spans);
    let through_fast = spans.len();
    spans.extend(cost_spans.into_iter().flatten());

    for keep in [spans.len(), through_fast, through_mode] {
        if spans_width(&spans[..keep]) <= budget {
            spans.truncate(keep);
            return Some(spans);
        }
    }
    None
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

fn print_line(out: &mut Stdout, line: &PaintLine) -> Result<()> {
    let palette = theme::palette();
    let background = match line.tone {
        Tone::UserPrompt => Some(palette.user_prompt_bg),
        Tone::ModelChange => Some(palette.model_change_bg),
        Tone::DiffAdded => Some(palette.diff_add_bg),
        Tone::DiffRemoved => Some(palette.diff_remove_bg),
        _ => None,
    };
    if let Some(background) = background {
        queue!(out, SetBackgroundColor(rgb_color(background)))?;
    }
    set_tone(out, line.prefix_tone)?;
    queue!(out, Print(&line.prefix))?;
    set_tone(out, line.tone)?;
    if line.bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    queue!(
        out,
        Print(&line.text),
        SetAttribute(Attribute::Reset),
        ResetColor
    )?;
    for span in &line.tail {
        if span.tone == Tone::CopyJoin {
            continue;
        }
        set_tone(out, span.tone)?;
        if span.bold {
            queue!(out, SetAttribute(Attribute::Bold))?;
        }
        queue!(
            out,
            Print(&span.text),
            SetAttribute(Attribute::Reset),
            ResetColor
        )?;
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

fn set_tone(out: &mut Stdout, tone: Tone) -> Result<()> {
    let palette = theme::palette();
    let color = match tone {
        Tone::Plain => rgb_color(palette.foreground),
        Tone::Muted | Tone::Thinking => rgb_color(palette.muted),
        Tone::Accent => rgb_color(palette.accent),
        Tone::User => rgb_color(palette.blue),
        Tone::Success => rgb_color(palette.success),
        Tone::Warning => rgb_color(palette.warning),
        Tone::Error => rgb_color(palette.error),
        Tone::Code => rgb_color(palette.code),
        Tone::EffortLow => rgb_color(palette.status.effort_low),
        Tone::EffortMedium => rgb_color(palette.status.effort_medium),
        Tone::EffortHigh => rgb_color(palette.status.effort_high),
        Tone::EffortXHigh => rgb_color(palette.status.effort_xhigh),
        Tone::EffortMax => rgb_color(palette.status.effort_max),
        Tone::Context => rgb_color(palette.status.context),
        Tone::StatusText => rgb_color(palette.status.text),
        Tone::StatusSeparator => rgb_color(palette.status.separator),
        Tone::UserPrompt => rgb_color(palette.foreground),
        Tone::ModelSol => rgb_color(palette.orange),
        Tone::ModelTerra => rgb_color(palette.pink),
        Tone::ModelLuna => rgb_color(palette.purple),
        Tone::Model55 => rgb_color(palette.blue),
        Tone::Border => rgb_color(palette.border),
        Tone::Branch => rgb_color(palette.status.branch),
        Tone::LimitFiveHour => rgb_color(palette.status.five_hour),
        Tone::LimitWeekly => rgb_color(palette.status.weekly),
        Tone::FastOn => rgb_color(palette.success),
        Tone::FastOff => rgb_color(palette.muted),
        Tone::ModelChange => rgb_color(palette.foreground),
        Tone::SyntaxComment => rgb_color(palette.syntax_comment),
        Tone::SyntaxString => rgb_color(palette.syntax_string),
        Tone::SyntaxKeyword => rgb_color(palette.syntax_keyword),
        Tone::SyntaxNumber => rgb_color(palette.syntax_number),
        Tone::SyntaxType => rgb_color(palette.syntax_type),
        Tone::SyntaxFunction => rgb_color(palette.syntax_function),
        Tone::InlineCode => rgb_color(palette.accent),
        Tone::DiffAdded => rgb_color(palette.diff_add),
        Tone::DiffRemoved => rgb_color(palette.diff_remove),
        Tone::DiffHeader => rgb_color(palette.diff_header),
        Tone::CopyJoin => Color::Reset,
    };
    queue!(out, SetForegroundColor(color))?;
    if tone == Tone::Thinking {
        queue!(out, SetAttribute(Attribute::Italic))?;
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
    fn composer_rows_do_not_emit_side_borders_or_copy_padding() {
        let mut editor = Editor::default();
        editor.set_text("wrapped-prompt-text");

        let (rows, _, _) = input_lines(&editor, 18, "", "placeholder", None);
        let prompt_rows = &rows[1..rows.len() - 1];

        assert!(prompt_rows.len() > 1);
        assert!(!rows[0].text.contains("Message"));
        assert!(rows[0].text.chars().all(|ch| ch == '─'));
        assert!(rows[0].tone == Tone::Muted);
        assert!(rows.last().is_some_and(|row| row.tone == Tone::Muted));
        assert_eq!(prompt_rows[0].prefix, "> ");
        assert_eq!(prompt_rows[1].prefix, "  ");
        assert!(!rows[0].text.contains(['╭', '╮', '╰', '╯']));
        assert!(
            rows.last()
                .is_some_and(|row| row.text.chars().all(|ch| ch == '─'))
        );
        assert!(prompt_rows.iter().all(|row| !row.prefix.contains('│')));
        assert!(prompt_rows.iter().all(|row| !row.text.ends_with(' ')));
        assert!(prompt_rows.iter().all(|row| !row.text.contains('│')));
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
        assert_eq!(lines[1].prefix, " ");
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
        let lines = code_line(
            "",
            Tone::Plain,
            "pub struct DevezClient { retries: 3, name: \"cli\", run: build() } // ready",
            "rust",
            120,
        );
        let line = &lines[0];
        let mut spans = vec![PaintSpan {
            text: line.text.clone(),
            tone: line.tone,
            bold: line.bold,
        }];
        spans.extend(line.tail.clone());

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

    /// The notice used to share the composer rule with the permission badge; it now
    /// owns the row directly above it so neither has to be squeezed.
    #[test]
    fn transient_notice_sits_on_its_own_row_above_the_composer_rule() {
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
                composer_notice: Some("Apps refreshed".to_owned()),
                composer_mode: None,
            },
            80,
        );

        let notice = frame
            .lines
            .iter()
            .position(|line| line.text == "Apps refreshed")
            .expect("notice row");
        assert_eq!(frame.lines[notice].tone, Tone::Success);
        assert!(frame.lines[notice + 1] == PaintLine::blank());
        assert!(!frame.lines[notice + 2].text.is_empty());
        assert!(frame.lines[notice + 2].text.chars().all(|ch| ch == '─'));
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
            Some("✶ Working… 2s · Esc to interrupt"),
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
            .position(|line| line.text.contains("Working…"))
            .expect("activity row");
        assert_eq!(frame.lines[activity].tone, Tone::Accent);
        // One blank row separates the activity line from the composer rule.
        assert!(frame.lines[activity + 1] == PaintLine::blank());
        assert!(!frame.lines[activity + 2].text.is_empty());
        assert!(frame.lines[activity + 2].text.chars().all(|ch| ch == '─'));
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
            fast_mode,
            cost: None,
        }
    }

    #[test]
    fn permission_mode_and_fast_flag_sit_inside_the_composer_rule() {
        let mode = test_mode("Full Access", ModeAccent::Danger, true);
        let line = input_top_line(50, "", Some(&mode));
        let texts = line
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rule_width(&line), 50);
        assert!(line.text.chars().all(|ch| ch == '─'));
        // Two blanks off the rule, the badge, then the rule resumes for two columns.
        assert_eq!(texts, ["  ", "Full Access", " · ", "Fast On", " ", "──"]);
        assert_eq!(line.tail[1].tone, Tone::Warning);
        assert_eq!(line.tail[3].tone, Tone::FastOn);
    }

    #[test]
    fn fast_off_is_toned_down_beside_the_permission_mode() {
        let mode = test_mode("Default", ModeAccent::Safe, false);
        let line = input_top_line(50, "", Some(&mode));
        let texts = line
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rule_width(&line), 50);
        assert_eq!(texts, ["  ", "Default", " · ", "Fast Off", " ", "──"]);
        assert_eq!(line.tail[3].tone, Tone::FastOff);
    }

    #[test]
    fn estimated_cost_rides_beside_the_permission_mode() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());
        let line = input_top_line(60, "", Some(&mode));
        let texts = line
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rule_width(&line), 60);
        assert_eq!(
            texts,
            [
                "  ",
                "Full Access",
                " · ",
                "Fast On",
                " · ",
                "$0.95",
                " ",
                "──"
            ]
        );
    }

    /// The cost is the first thing to go: it is the least load-bearing segment.
    #[test]
    fn the_cost_is_dropped_before_the_fast_flag() {
        let mut mode = test_mode("Full Access", ModeAccent::Danger, true);
        mode.cost = Some("$0.95".to_owned());
        let line = input_top_line(32, "", Some(&mode));
        let texts = line
            .tail
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(rule_width(&line), 32);
        assert_eq!(texts, ["  ", "Full Access", " · ", "Fast On", " ", "──"]);
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
        assert_eq!(texts, ["  ", "Full Access", " ", "──"]);
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
        assert!(
            frame.lines[suggestion_end + 1]
                .text
                .chars()
                .all(|ch| ch == '─')
        );
    }

    #[test]
    fn conversation_blocks_hide_speaker_labels() {
        let user = Block::new(BlockKind::User, "You", "hello");
        let assistant = Block::new(BlockKind::Assistant, "Codex", "hi");

        let user_lines = block_lines(&user, 80);
        let assistant_lines = block_lines(&assistant, 80);

        assert_eq!(user_lines[0].prefix, "❯ ");
        assert_eq!(user_lines[0].text, "hello");
        assert!(user_lines[0].prefix_tone == Tone::Plain);
        assert!(user_lines[0].tone == Tone::UserPrompt);
        assert!(user_lines[0].bold);
        assert_eq!(assistant_lines[0].prefix, "● ");
        assert_eq!(assistant_lines[0].text, "hi");
        assert!(user_lines.iter().all(|line| line.text != "You"));
        assert!(assistant_lines.iter().all(|line| line.text != "Codex"));
    }

    #[test]
    fn tool_output_keeps_five_rows_and_counts_the_rest() {
        let body = (1..=12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = block_lines(&Block::new(BlockKind::Tool, "Bash · ls", body), 200);

        let texts = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                "Bash · ls",
                "line 1",
                "line 2",
                "line 3",
                "line 4",
                "line 5",
                "… +7 lines"
            ]
        );
        assert_eq!(lines[6].tone, Tone::Muted);
    }

    #[test]
    fn short_tool_output_is_shown_whole_without_a_count() {
        let lines = block_lines(&Block::new(BlockKind::Tool, "Bash · pwd", "/src\n"), 200);

        let texts = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(texts, ["Bash · pwd", "/src"]);
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
    fn status_line_is_trimmed_to_terminal_width() {
        let line = status_line_row(
            Some(StatusLineView {
                branch: Some("main".to_owned()),
                model: "GPT-5.6 Codex".to_owned(),
                effort: "xhigh".to_owned(),
                context: Some("ctx: 45k/256k (18%)".to_owned()),
                five_hour_percent: Some(12),
                weekly_percent: Some(34),
                notice: Some("connected".to_owned()),
            }),
            "",
            32,
        );
        let width = UnicodeWidthStr::width(line.text.as_str())
            + line
                .tail
                .iter()
                .map(|span| UnicodeWidthStr::width(span.text.as_str()))
                .sum::<usize>();

        assert!(width <= 32);
        assert!(line.text.starts_with("main"));
    }

    #[test]
    fn status_line_keeps_an_empty_branch_slot_at_the_far_left() {
        let line = status_line_row(
            Some(StatusLineView {
                branch: None,
                model: "GPT-5.6 Sol".to_owned(),
                effort: "high".to_owned(),
                context: None,
                five_hour_percent: None,
                weekly_percent: None,
                notice: None,
            }),
            "",
            80,
        );

        assert_eq!(line.text, "--");
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
                lines.iter().any(|line| painted(line)
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
            },
            SuggestionView {
                command: "/effort".to_owned(),
                description: "Set reasoning effort".to_owned(),
                selected: false,
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
    fn panel_overlay_keeps_its_border_when_a_row_folds() {
        // An unbreakable run far wider than the terminal, like an OAuth URL.
        let long = "a".repeat(400);
        let frame = overlay_frame(
            &[],
            OverlayView {
                title: "Sign in to ChatGPT".to_owned(),
                lines: vec![OverlayLine {
                    text: long,
                    selected: false,
                    muted: true,
                }],
                hint: "Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
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
    fn a_panel_pads_inside_its_borders() {
        let frame = overlay_frame(
            &[],
            OverlayView {
                title: "Resume session".to_owned(),
                lines: vec![OverlayLine {
                    text: "yesterday's session".to_owned(),
                    selected: true,
                    muted: false,
                }],
                hint: "Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
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
                title: "Resume session · 1 · this folder".to_owned(),
                lines: vec![OverlayLine {
                    text: "yesterday's session  ·  2h ago".to_owned(),
                    selected: true,
                    muted: false,
                }],
                hint: "↑↓ navigate  Enter resume  Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: Some(&editor),
                input_label: "Search",
                input_placeholder: "Search by name…",
            },
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
    }

    #[test]
    fn picker_overlay_uses_restrained_borderless_chrome() {
        let frame = overlay_frame(
            &[],
            OverlayView {
                title: "Select model".to_owned(),
                lines: vec![
                    OverlayLine {
                        text: "GPT-5.6-Sol".to_owned(),
                        selected: true,
                        muted: false,
                    },
                    OverlayLine {
                        text: "GPT-5.3-Codex-Spark".to_owned(),
                        selected: false,
                        muted: false,
                    },
                ],
                hint: "↑↓ model   Enter select".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            },
            StatusArea {
                fallback: String::new(),
                line: None,
                composer_notice: None,
                composer_mode: None,
            },
            80,
        );

        assert_eq!(frame.lines[0].text, "Select model");
        assert_eq!(frame.lines[2].prefix, "  ❯ ");
        assert!(frame.lines[2].tone == Tone::ModelSol);
        assert!(frame.lines.iter().all(|line| {
            !line.text.contains(['╭', '╮', '╰', '╯', '│'])
                && !line.prefix.contains(['╭', '╮', '╰', '╯', '│'])
        }));
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
