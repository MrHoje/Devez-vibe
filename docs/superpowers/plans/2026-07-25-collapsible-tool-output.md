# Collapsible Tool Output Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Collapse Bash output to its heading by default and toggle the full output by clicking that heading in fullscreen mode.

**Architecture:** Each `Block` receives a stable process-local ID that survives active-to-completed command transitions. The renderer stores expanded IDs, tags painted Bash heading rows for hit testing, and rewraps the fullscreen transcript after a click; application state and protocol payloads remain unchanged.

**Tech Stack:** Rust 2024, crossterm 0.29, existing unit tests in `src/state.rs`, `src/renderer.rs`, and `src/main.rs`.

## Global Constraints

- New Bash blocks start collapsed.
- A left click on a visible Bash heading toggles only that block.
- Expanded Bash blocks show every non-empty output row.
- Inline mode remains collapsed and does not capture mouse clicks.
- Plan steps, reasoning, live activity/progress, non-Bash tools, file changes,
  and messages remain visible and unchanged.
- Only the stdout/stderr body beneath a Bash heading is collapsed.
- Existing wheel scrolling remains unchanged.
- Preserve unrelated working-tree edits.

---

### Task 1: Stable Block Identity

**Files:**
- Modify: `src/renderer.rs:94-130`
- Modify: `src/state.rs:4920-4990`
- Test: `src/state.rs` test module

**Interfaces:**
- Produces: `Block::id() -> u64`
- Produces: `Block::adopt_id(&mut self, source: &Block)`
- Consumes: existing `Block::new` call sites without signature changes

- [ ] **Step 1: Write a failing state test**

Add a test that starts a `commandExecution` item, records the active block ID,
completes the same protocol item, and asserts that its committed block has the
same ID:

```rust
#[test]
fn command_block_identity_survives_active_to_completed_transition() {
    let mut state = test_state();
    state.upsert_active(&json!({
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
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```powershell
cargo test command_block_identity_survives_active_to_completed_transition
```

Expected: compilation fails because `Block::id` does not exist.

- [ ] **Step 3: Add generated IDs to `Block`**

Use an atomic counter without changing existing constructors:

```rust
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_BLOCK_ID: AtomicU64 = AtomicU64::new(1);

pub struct Block {
    id: u64,
    pub kind: BlockKind,
    pub title: String,
    pub body: String,
}

impl Block {
    pub fn new(kind: BlockKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            id: NEXT_BLOCK_ID.fetch_add(1, Ordering::Relaxed),
            kind,
            title: title.into(),
            body: body.into(),
        }
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn adopt_id(&mut self, source: &Self) {
        self.id = source.id;
    }
}
```

In `upsert_active`, retain an existing active block's ID before replacing the
block. In `complete_item`, remove the active item into a local variable and call
`completed.adopt_id(&active.block)` before pushing the completed block.

- [ ] **Step 4: Run focused and state tests**

Run:

```powershell
cargo test command_block_identity_survives_active_to_completed_transition
cargo test state::tests
```

Expected: both commands pass.

- [ ] **Step 5: Commit the identity change**

```powershell
git add -- src/renderer.rs src/state.rs
git commit -m "refactor: preserve tool block identity"
```

---

### Task 2: Collapsed and Expanded Bash Rendering

**Files:**
- Modify: `src/renderer.rs:260-540`
- Modify: `src/renderer.rs:820-930`
- Modify: `src/renderer.rs:1694-1750`
- Modify: `src/renderer.rs:1922-1980`
- Test: `src/renderer.rs:4056-4120`

**Interfaces:**
- Consumes: `Block::id() -> u64`
- Produces: `Renderer::toggle_tool_at(row: u16) -> bool`
- Produces: `block_lines_with_expansion(block: &Block, width: u16, expanded: bool) -> Vec<PaintLine>`
- Stores: `Renderer::expanded_tools: HashSet<u64>`
- Stores: `PaintLine::tool_heading: Option<u64>`

- [ ] **Step 1: Replace preview expectations with failing collapse tests**

Add focused tests:

```rust
#[test]
fn bash_output_is_collapsed_to_its_heading_by_default() {
    let block = Block::new(BlockKind::Tool, "Bash · rg TODO · exit 0 · 12ms", "one\ntwo");
    let lines = block_lines(&block, 200);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].prefix, "▸ ");
    assert_eq!(lines[0].text, "Bash · rg TODO · exit 0 · 12ms");
    assert_eq!(lines[0].tool_heading, Some(block.id()));
}

#[test]
fn expanded_bash_output_shows_every_non_empty_row() {
    let block = Block::new(BlockKind::Tool, "Bash · rg TODO", "one\n\n two\nthree");
    let lines = block_lines_with_expansion(&block, 200, true);
    let texts = lines.iter().map(|line| line.text.as_str()).collect::<Vec<_>>();
    assert_eq!(texts, ["Bash · rg TODO", "one", " two", "three"]);
    assert_eq!(lines[0].prefix, "▾ ");
}

#[test]
fn non_bash_tools_keep_the_existing_tail_preview() {
    let block = Block::new(BlockKind::Tool, "MCP · server › tool", "one\ntwo");
    let lines = block_lines(&block, 200);
    assert_eq!(lines.iter().map(|line| line.text.as_str()).collect::<Vec<_>>(),
               ["MCP · server › tool", "one", "two"]);
    assert_eq!(lines[0].prefix, "● ");
}

#[test]
fn every_wrapped_bash_heading_row_is_clickable() {
    let block = Block::new(BlockKind::Tool, format!("Bash · {}", "x".repeat(100)), "");
    let lines = block_lines(&block, 20);
    assert!(lines.len() > 1);
    assert!(lines.iter().all(|line| line.tool_heading == Some(block.id())));
}
```

- [ ] **Step 2: Run the renderer tests and verify RED**

Run:

```powershell
cargo test renderer::tests::bash_output_is_collapsed_to_its_heading_by_default
cargo test renderer::tests::expanded_bash_output_shows_every_non_empty_row
```

Expected: the first test sees output rows and the second cannot find
`block_lines_with_expansion`.

- [ ] **Step 3: Implement Bash-specific rendering**

Add `tool_heading: Option<u64>` to `PaintLine`, initializing it to `None` in all
constructors. Split Bash rendering from the existing generic tool preview:

```rust
fn is_bash_block(block: &Block) -> bool {
    matches!(block.kind, BlockKind::Tool | BlockKind::Warning)
        && block.title.starts_with("Bash ·")
}

fn bash_lines(block: &Block, width: u16, expanded: bool) -> Vec<PaintLine> {
    let marker = if expanded { "▾ " } else { "▸ " };
    let mut lines = wrapped_line(marker, Tone::User, &block.title, Tone::Plain, true, width);
    for line in &mut lines {
        line.tool_heading = Some(block.id());
    }
    if expanded {
        for row in block.body.lines().filter(|row| !row.trim().is_empty()) {
            lines.extend(wrapped_line("  ", Tone::Muted, row, Tone::Muted, false, width));
        }
    }
    lines
}
```

Make `block_lines` call `block_lines_with_expansion(..., false)`. Route Bash
`Tool` and `Warning` blocks through `bash_lines`; keep the current five-row tail
logic for non-Bash `Tool` blocks.

- [ ] **Step 4: Add failing renderer toggle tests**

Construct a fullscreen `Renderer`, put two Bash heading `PaintLine`s into
`previous_lines`, and assert:

```rust
assert!(renderer.toggle_tool_at(0));
assert!(renderer.expanded_tools.contains(&first.id()));
assert!(!renderer.expanded_tools.contains(&second.id()));
assert!(!renderer.toggle_tool_at(2));
```

Also construct an inline renderer and assert `toggle_tool_at(0)` returns false.

- [ ] **Step 5: Run the toggle tests and verify RED**

Run:

```powershell
cargo test renderer::tests::clicking_a_bash_heading_toggles_only_that_block
cargo test renderer::tests::inline_renderer_ignores_tool_heading_clicks
```

Expected: compilation fails because `toggle_tool_at` and `expanded_tools` do not
exist.

- [ ] **Step 6: Implement renderer expansion state and rewrapping**

Add `HashSet<u64>` to `Renderer`. Pass it to fullscreen history and live-frame
layout via `block_lines_with_expansion(block, width, set.contains(&block.id()))`.
Implement:

```rust
pub fn toggle_tool_at(&mut self, row: u16) -> bool {
    if self.mode != RenderMode::Fullscreen {
        return false;
    }
    let Some(id) = self.previous_lines
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
```

Clear `expanded_tools` in `clear_screen`. Ensure `normal_frame` and
`overlay_frame` render live Bash blocks with the same expanded-ID set.

Add a resize/rewrap test that expands a block, calls `rewrap` at two widths, and
asserts its ID remains in `expanded_tools` and both layouts contain its body.
Retain the existing reasoning, plan, activity, file-change, and non-Bash tool
tests unchanged so their continued visibility remains covered.

- [ ] **Step 7: Run all renderer tests**

Run:

```powershell
cargo test renderer::tests
```

Expected: all tests pass; obsolete Bash tail-preview assertions are removed or
retargeted to a non-Bash tool block.

- [ ] **Step 8: Commit renderer behavior**

```powershell
git add -- src/renderer.rs
git commit -m "feat: collapse bash output by default"
```

---

### Task 3: Route Left Clicks to the Renderer

**Files:**
- Modify: `src/main.rs:540-560`
- Test: `src/main.rs` test module

**Interfaces:**
- Consumes: `Renderer::toggle_tool_at(row: u16) -> bool`
- Produces: `mouse_request(event: &MouseEvent) -> MouseRequest`
- Produces: private `MouseRequest::{Scroll(isize), ToggleTool(u16), None}`

- [ ] **Step 1: Write failing mouse-routing tests**

Extract mouse classification into a pure helper and test left-button down,
wheel, and unrelated mouse events:

```rust
assert_eq!(
    mouse_request(&MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 7,
        modifiers: KeyModifiers::NONE,
    }),
    MouseRequest::ToggleTool(7)
);
```

Assert scroll-up maps to `MouseRequest::Scroll(WHEEL_ROWS)` and mouse movement
maps to `MouseRequest::None`.

- [ ] **Step 2: Run the mouse-routing test and verify RED**

Run:

```powershell
cargo test mouse_click_toggles_only_tool_headings
```

Expected: compilation fails because `mouse_request` does not exist.

- [ ] **Step 3: Implement the mouse helper**

Import `MouseButton` and `MouseEvent`, then classify mouse events with:

```rust
#[derive(Debug, PartialEq, Eq)]
enum MouseRequest {
    Scroll(isize),
    ToggleTool(u16),
    None,
}

fn mouse_request(mouse: &MouseEvent) -> MouseRequest {
    match mouse.kind {
        MouseEventKind::ScrollUp => MouseRequest::Scroll(WHEEL_ROWS),
        MouseEventKind::ScrollDown => MouseRequest::Scroll(-WHEEL_ROWS),
        MouseEventKind::Down(MouseButton::Left) => MouseRequest::ToggleTool(mouse.row),
        _ => MouseRequest::None,
    }
}
```

In the event loop, map `Scroll(delta)` to `renderer.scroll(delta)`,
`ToggleTool(row)` to `renderer.toggle_tool_at(row)`, and `None` to `false`.
Do not enable mouse capture in inline mode.

- [ ] **Step 4: Run focused and full verification**

Run:

```powershell
cargo test mouse_click_toggles_only_tool_headings
cargo test
cargo fmt --check
```

Expected: all tests pass and formatting is clean.

- [ ] **Step 5: Commit click routing**

```powershell
git add -- src/main.rs
git commit -m "feat: toggle bash details on click"
```
