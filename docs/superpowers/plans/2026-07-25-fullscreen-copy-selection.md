# Fullscreen-owned Copy Selection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace global clipboard polling with Claude-style, session-owned fullscreen mouse selection and copy feedback.

**Architecture:** A focused `selection` module owns screen-cell ranges and Unicode-aware text extraction. `Renderer` owns selection lifecycle and highlighting over its current fullscreen `PaintLine` buffer, while `main` routes mouse events and performs clipboard writes through the existing `Action::Copy` path.

**Tech Stack:** Rust 2024, crossterm 0.29 mouse reporting, arboard 3.6, unicode-width 0.2, existing unit tests.

## Global Constraints

- Another process or Codex session changing the clipboard must never change `dvz` UI.
- Fullscreen owns left-drag selection; inline selection remains terminal-native.
- A same-cell down/up is a click and remains available to tool-heading toggles.
- Shift-modified mouse input is not treated as application selection.
- Wheel input scrolls the transcript and never reaches prompt history.
- `/copy` retains its current behavior.
- Preserve all unrelated working-tree edits.

---

### Task 1: Pure Selection Model and Text Extraction

**Files:**
- Create: `src/selection.rs`
- Modify: `src/main.rs`
- Test: `src/selection.rs`

**Interfaces:**
- Produces: `CellPosition { column: u16, row: u16 }`
- Produces: `Selection::begin`, `Selection::update`, `Selection::finish`, `Selection::clear`
- Produces: `SelectionFinish::{Copy(CellRange), Click(u16), None}`
- Produces: `CopyLine { text: String, join_next: bool, marker_width: usize }`
- Produces: `extract_text(lines: &[CopyLine], range: CellRange) -> String`
- Produces: `CellRange::columns_for_row(row, line_width) -> Option<Range<usize>>`

- [ ] **Step 1: Add failing lifecycle tests**

Test forward and backward drags, inclusive endpoints, same-cell clicks, and
clearing. A drag from `(2, 0)` to `(4, 0)` must normalize to columns `2..5`; a
down/up at `(2, 7)` must return `Click(7)`.

- [ ] **Step 2: Run lifecycle tests and verify RED**

Run:

```powershell
cargo test selection::tests:: -- --nocapture
```

Expected: compilation fails because `mod selection` and its types do not exist.

- [ ] **Step 3: Implement the minimal lifecycle model**

Use a two-point selection with a `moved` flag. Clamp endpoints only when the
renderer supplies its current screen bounds; keep the pure model independent of
terminal state.

- [ ] **Step 4: Add failing extraction tests**

Cover:

```text
single line: "abcdef", 2..5 -> "cde"
wide text: "A한B", selecting either cell of 한 -> "한"
combining text: "e\u{301}x", selecting e -> "e\u{301}"
joined wrap: "hello " + "world" with join_next -> "hello world"
independent rows -> newline
complete "● " marker selection -> marker omitted
partial marker selection -> selected visible glyph retained
```

- [ ] **Step 5: Run extraction tests and verify RED**

Run:

```powershell
cargo test selection::tests::extract_ -- --nocapture
```

Expected: failures show unimplemented Unicode cell slicing and row joining.

- [ ] **Step 6: Implement Unicode-aware extraction**

Iterate scalar values with `UnicodeWidthChar`, attach zero-width scalars to the
preceding selected cell, include a wide scalar when any occupied cell
intersects the selected range, and use `join_next` to choose direct
concatenation versus `\n`.

- [ ] **Step 7: Run the selection module tests**

Run:

```powershell
cargo test selection::tests:: -- --nocapture
```

Expected: all selection tests pass.

---

### Task 2: Fullscreen Renderer Selection and Highlighting

**Files:**
- Modify: `src/renderer.rs`
- Test: `src/renderer.rs`

**Interfaces:**
- Consumes: selection module types from Task 1.
- Produces: `Renderer::begin_selection(column, row) -> bool`
- Produces: `Renderer::update_selection(column, row) -> bool`
- Produces: `Renderer::finish_selection(column, row) -> SelectionResult`
- Produces: `Renderer::clear_selection() -> bool`
- Produces: `SelectionResult::{Copy(String), Click(u16), None}`

- [ ] **Step 1: Add failing renderer selection tests**

Build a fullscreen renderer with explicit `previous_lines`. Assert that a drag
returns reconstructed text, a same-cell click returns its row, inline methods
return `None`, and scrolling clears a completed selection.

- [ ] **Step 2: Run renderer selection tests and verify RED**

Run:

```powershell
cargo test renderer::tests::fullscreen_selection -- --nocapture
```

Expected: compilation fails because the renderer selection methods do not exist.

- [ ] **Step 3: Implement renderer lifecycle and screen conversion**

Store `Selection` plus the last painted `CellRange` in `Renderer`. Convert each
`PaintLine` into visible text (`prefix + text + non-CopyJoin tails`),
`copy_joins_next`, and complete decorative marker width. Clamp row/column
coordinates against `previous_lines`.

- [ ] **Step 4: Add failing highlight chunk tests**

Assert selection splits `"ab한cd"` into unselected/selected chunks by terminal
cell range without splitting the wide character or dropping combining marks.

- [ ] **Step 5: Run highlight tests and verify RED**

Run:

```powershell
cargo test renderer::tests::selection_chunks -- --nocapture
```

Expected: failure because selection-aware chunks are absent.

- [ ] **Step 6: Paint selection using reverse video**

Add a selection-aware print path for fullscreen rows. Repaint a row whenever
its old and new selection column ranges differ, even if its `PaintLine` is
unchanged. Preserve existing tone, bold, syntax color, and background handling.

- [ ] **Step 7: Clear stale selection at renderer boundaries**

Clear on successful transcript scroll, relayout/reset, screen clear, and when a
selected row's content changes during repaint. Preserve a completed transcript
selection when an unrelated composer notice changes.

- [ ] **Step 8: Run renderer tests**

Run:

```powershell
cargo test renderer::tests -- --nocapture
```

Expected: all renderer tests pass, including concurrent collapsible-tool tests.

---

### Task 3: Terminal Mouse Lifecycle and Event Routing

**Files:**
- Modify: `src/renderer.rs`
- Modify: `src/main.rs`
- Test: `src/renderer.rs`
- Test: `src/main.rs`

**Interfaces:**
- Consumes: renderer selection methods from Task 2.
- Produces: `MouseRequest::{Scroll, SelectionStart, SelectionUpdate, SelectionEnd, None}`

- [ ] **Step 1: Add failing terminal lifecycle tests**

Update fullscreen byte expectations to require mouse capture after alternate
screen entry and mouse release before alternate-scroll restoration. Assert
inline terminal setup remains free of mouse control sequences.

- [ ] **Step 2: Run lifecycle tests and verify RED**

Run:

```powershell
cargo test renderer::tests::fullscreen_ -- --nocapture
```

Expected: byte mismatch because mouse capture is not enabled.

- [ ] **Step 3: Enable fullscreen-only mouse capture**

Use crossterm `EnableMouseCapture` and `DisableMouseCapture` in the fullscreen
enter/leave helpers. Preserve the existing alternate-scroll save/disable and
restore ordering.

- [ ] **Step 4: Replace click-only mouse tests with failing selection routing**

Assert left down/drag/up route cell coordinates, wheel routes scroll, moved and
unsupported buttons are ignored, and any Shift-modified selection event returns
`None`.

- [ ] **Step 5: Run routing tests and verify RED**

Run:

```powershell
cargo test mouse_requests_ -- --nocapture
```

Expected: mismatch against the current `ToggleTool`-on-down behavior.

- [ ] **Step 6: Implement event-loop routing**

Route down/drag/up into `Renderer`. Map `SelectionResult::Copy(text)` to
`Action::Copy(text)`, same-cell `Click(row)` to `toggle_tool_at(row)`, and
selection repaints to `Action::Tick(true)`. Clear selection before ordinary key,
paste, resize, and wheel handling.

- [ ] **Step 7: Run focused integration tests**

Run:

```powershell
cargo test mouse_requests_ -- --nocapture
cargo test copy_notice -- --nocapture
```

Expected: all focused tests pass.

---

### Task 4: Remove Global Polling and Verify

**Files:**
- Modify: `src/main.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Removes: `ClipboardWatcher`
- Removes: direct `clipboard-win` dependency
- Retains: `arboard::Clipboard` for local clipboard writes

- [ ] **Step 1: Add a source-level regression assertion**

Add a focused test that exercises every `MouseRequest` path without reading the
OS clipboard. Keep global clipboard access exclusively inside `Action::Copy`.

- [ ] **Step 2: Remove polling**

Delete `ClipboardWatcher`, its activity-tick branch, and the direct
`clipboard-win` dependency. Regenerate the lockfile through Cargo.

- [ ] **Step 3: Run formatting and full verification**

Run:

```powershell
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Expected: all commands succeed without warnings.

- [ ] **Step 4: Inspect the focused diff**

Run:

```powershell
git diff --check
git diff -- src/selection.rs src/renderer.rs src/main.rs Cargo.toml Cargo.lock
```

Expected: only selection/copy changes plus preserved concurrent renderer work.
