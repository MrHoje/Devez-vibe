# Bash Heading Hover Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every clickable fullscreen Bash heading a text-only hover background while preserving its existing click, selection, and scroll behavior.

**Architecture:** Mouse-move events are routed to `Renderer`, which resolves the pointed screen cell against existing `PaintLine::tool_heading` metadata and stores the hovered Bash block ID. Fullscreen painting compares current and previously painted hover state so only affected rows repaint, and applies a theme-specific background to heading text while leaving the disclosure prefix untouched.

**Tech Stack:** Rust, crossterm mouse events and ANSI painting, existing renderer unit tests.

## Global Constraints

- Only expandable Bash headings receive hover styling.
- The `▸` or `▾` prefix and unused row space remain unchanged.
- Wrapped rows belonging to one Bash heading share its hover state.
- Inline render mode remains unchanged.
- Existing click-to-toggle, drag-selection, and scroll behavior must remain intact.
- Preserve unrelated modifications already present in the working tree.

---

### Task 1: Track the Bash heading under the pointer

**Files:**
- Modify: `src/main.rs`
- Modify: `src/renderer.rs`
- Test: `src/main.rs`
- Test: `src/renderer.rs`

**Interfaces:**
- Consumes: `PaintLine::tool_heading: Option<u64>` and mouse cell coordinates.
- Produces: `MouseRequest::Hover(u16, u16)` and `Renderer::hover_tool_at(column: u16, row: u16) -> bool`.

- [ ] **Step 1: Write failing mouse-routing and renderer-state tests**

```rust
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

#[test]
fn bash_hover_tracks_only_the_heading_text_cells() {
    let block = Block::new(BlockKind::Tool, "Bash · cargo test", "");
    let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
    renderer.previous_lines = block_lines(&block, 80);

    assert!(!renderer.hover_tool_at(0, 0)); // disclosure arrow
    assert!(renderer.hover_tool_at(2, 0)); // Bash text
    assert_eq!(renderer.hovered_tool, Some(block.id()));
    assert!(!renderer.hover_tool_at(3, 0)); // unchanged inside same heading
    assert!(renderer.hover_tool_at(79, 0)); // leaves visible text
    assert_eq!(renderer.hovered_tool, None);
}
```

- [ ] **Step 2: Run the focused tests and confirm RED**

Run:

```powershell
cargo test mouse_move_routes_hover_coordinates
cargo test bash_hover_tracks_only_the_heading_text_cells
```

Expected: compilation or assertion failure because `MouseRequest::Hover`, `hovered_tool`, and `hover_tool_at` do not exist.

- [ ] **Step 3: Add minimal hover state and event routing**

Add `hovered_tool: Option<u64>` to `Renderer`, initialize it to `None`, and implement:

```rust
pub fn hover_tool_at(&mut self, column: u16, row: u16) -> bool {
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
    let changed = hovered != self.hovered_tool;
    self.hovered_tool = hovered;
    changed
}
```

Add `Hover(u16, u16)` to `MouseRequest`, map `MouseEventKind::Moved`, and route it in the input loop:

```rust
MouseRequest::Hover(column, row) => {
    Action::Tick(renderer.hover_tool_at(column, row))
}
```

- [ ] **Step 4: Run focused tests and confirm GREEN**

Run:

```powershell
cargo test mouse_move_routes_hover_coordinates
cargo test bash_hover_tracks_only_the_heading_text_cells
cargo test clicking_a_bash_heading_toggles_only_that_block
cargo test fullscreen_selection_copies_the_current_screen_and_preserves_clicks
```

Expected: all focused tests pass.

### Task 2: Paint a text-only theme hover background

**Files:**
- Modify: `src/theme.rs`
- Modify: `src/renderer.rs`
- Test: `src/renderer.rs`
- Test: `src/theme.rs`

**Interfaces:**
- Consumes: `Renderer::hovered_tool` and `PaintLine::tool_heading`.
- Produces: `ThemePalette::hover_bg: Rgb` and `print_line_with_selection(..., hovered: bool)`.

- [ ] **Step 1: Write a failing ANSI painting test**

```rust
#[test]
fn bash_hover_background_starts_after_the_disclosure_arrow() {
    theme::set_current(ThemeKind::Dark);
    let block = Block::new(BlockKind::Tool, "Bash · cargo test", "");
    let line = block_lines(&block, 80).remove(0);
    let mut output = Vec::new();

    print_line_with_selection(&mut output, &line, None, true).expect("hover paint");
    let painted = String::from_utf8(output).expect("utf-8 escapes");
    let hover = theme::palette().hover_bg;
    let hover_escape = format!("\x1b[48;2;{};{};{}m", hover.0, hover.1, hover.2);

    let arrow = painted.find("▸ ").expect("arrow");
    let background = painted.find(&hover_escape).expect("hover background");
    let title = painted.find("Bash").expect("title");
    assert!(arrow < background);
    assert!(background < title);
}
```

- [ ] **Step 2: Run the painting test and confirm RED**

Run:

```powershell
cargo test bash_hover_background_starts_after_the_disclosure_arrow
```

Expected: compilation failure because `hover_bg` and the `hovered` print argument do not exist.

- [ ] **Step 3: Add theme hover colors and targeted painting**

Add `hover_bg: Rgb` to `ThemePalette` with restrained values:

```rust
// MINIMAL
hover_bg: Rgb(0xE8, 0xEE, 0xF7),
// SOFT
hover_bg: Rgb(0xE7, 0xE0, 0xD7),
// DARK
hover_bg: Rgb(0x32, 0x32, 0x31),
```

Extend `print_line_with_selection` with `hovered: bool`. Paint the prefix with the row's normal background, set `hover_bg` immediately before the heading text, pass it as the text run's restoration background, then reset before tail spans. Keep `print_line` passing `false`.

Add `painted_hovered_tool: Option<u64>` to `Renderer`, initialize it to `None`, and reset it with other transient paint state. In `paint_screen`, calculate:

```rust
let hovered = line.tool_heading.is_some()
    && line.tool_heading == self.hovered_tool;
let previously_hovered = self.previous_lines.get(row).is_some_and(|previous| {
    previous.tool_heading.is_some()
        && previous.tool_heading == self.painted_hovered_tool
});
```

Include hover changes in the repaint condition, pass `hovered` into `print_line_with_selection`, and store `painted_hovered_tool = self.hovered_tool` after painting.

- [ ] **Step 4: Verify hover painting and theme contrast**

Add each palette's `hover_bg` to the existing theme contrast test:

```rust
assert!(
    contrast_ratio(palette.foreground, palette.hover_bg) >= 4.5,
    "{kind:?} hover text contrast"
);
```

Run:

```powershell
cargo test bash_hover_background_starts_after_the_disclosure_arrow
cargo test every_theme_has_readable_core_contrast
```

Expected: both tests pass.

### Task 3: Regression verification

**Files:**
- Verify: `src/main.rs`
- Verify: `src/renderer.rs`
- Verify: `src/theme.rs`

**Interfaces:**
- Consumes: completed hover tracking and painting.
- Produces: verified build and formatting status.

- [ ] **Step 1: Run formatter and focused behavior tests**

Run:

```powershell
cargo fmt --check
cargo test bash_hover
cargo test clicking_a_bash_heading_toggles_only_that_block
cargo test fullscreen_selection_copies_the_current_screen_and_preserves_clicks
```

Expected: all commands exit successfully.

- [ ] **Step 2: Run build and full tests**

Run:

```powershell
cargo build
cargo test
```

Expected: build succeeds. If unrelated concurrent work still causes the known fullscreen mouse-mode tests to fail, report those exact failures separately without changing unrelated code.

- [ ] **Step 3: Inspect only the scoped diff**

Run:

```powershell
git diff --check -- src/main.rs src/renderer.rs src/theme.rs
git diff -- src/main.rs src/renderer.rs src/theme.rs
```

Expected: no whitespace errors; hover changes are limited to mouse routing, renderer hover state/painting, theme color, and tests.

- [ ] **Step 4: Preserve the mixed working tree**

Do not commit the source files as one feature commit because they already contain unrelated user changes. Report that implementation remains on `main` in the working tree.
