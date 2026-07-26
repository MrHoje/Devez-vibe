# Shell Display Modes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a global clickable Shell visibility control with Hide, Collapse, and five-row Expand modes, keeping final Shell summaries in their original transcript position.

**Architecture:** `AppState` owns `ShellDisplayMode` and provides it to `ComposerMode`. The renderer paints the mode badge and uses it for all transcript rendering. A Shell batch emits a stable running anchor and later replaces that anchor with its completed summary by id.

**Tech Stack:** Rust, crossterm, existing unit tests in `renderer.rs`, `state.rs`, and `main.rs`.

## Global Constraints

- Default mode: `Collapse`.
- Badge order: estimated cost, Shell mode, permission mode, fast flag.
- Badge cycle: Hide → Collapse → Expand → Hide.
- Narrow widths remove estimated cost before Shell mode or permission mode.
- Expand allows five painted output rows across a complete Shell group in command order.
- Final Shell summary keeps its initial running anchor id and position.

---

### Task 1: Add the display mode state and clickable badge

**Files:**
- Modify: `src/renderer.rs: ComposerMode, Pick, input_top_line, fitting_badge_spans, tests`
- Modify: `src/state.rs: AppState, composer_mode`
- Modify: `src/main.rs: pick_action, tests`

**Interfaces:**
- Produces `ShellDisplayMode::{Hide, Collapse, Expand}` with `label()` and `next()`.
- Produces `Pick::ShellDisplayMode`, handled by `AppState::cycle_shell_display_mode()`.

- [ ] **Step 1: Write failing tests**

```rust
assert_eq!(pick_on(&line, "Shell: Collapse"), Some(Pick::ShellDisplayMode));
pick_action(&mut state, Pick::ShellDisplayMode);
assert_eq!(state.shell_display_mode(), ShellDisplayMode::Expand);
```

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test shell_badge_sits_between_cost_and_permission_mode clicking_shell_badge_cycles_the_global_mode`

Expected: compile failure because the display mode and pick do not exist.

- [ ] **Step 3: Add the minimal state and rendering data**

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellDisplayMode { Hide, #[default] Collapse, Expand }
```

Add the mode to `AppState` and `ComposerMode`, paint `Shell: <label>` after cost, attach its `Pick`, and make the badge ladder remove cost first on narrow terminals.

- [ ] **Step 4: Verify the tests pass**

Run: `cargo test shell_badge_sits_between_cost_and_permission_mode clicking_shell_badge_cycles_the_global_mode`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add src/renderer.rs src/state.rs src/main.rs
git commit -m "feat: add shell display mode control"
```

### Task 2: Render Hide, Collapse, and five-row Expand previews

**Files:**
- Modify: `src/renderer.rs: render paths, rewrap, block_group_lines_with_expansion, bash_lines, tests`

**Interfaces:**
- Consumes `ShellDisplayMode` and per-block expansion state.
- Produces mode-aware Shell group rows.

- [ ] **Step 1: Write failing tests**

```rust
assert!(shell_group_lines(&group, 80, ShellDisplayMode::Hide, false).is_empty());
let lines = shell_group_lines(&group, 80, ShellDisplayMode::Expand, false);
assert_eq!(lines.iter().filter(|line| line.prefix == "    ").count(), 5);
```

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test hidden_shell_group_paints_no_rows expanded_shell_group_caps_output_at_five_painted_rows_across_children`

Expected: compile failure because the mode-aware helper does not exist.

- [ ] **Step 3: Add mode-aware Shell painting**

Keep Collapse as the existing one-line ellipsized summary. Hide returns no rows. Expand renders the summary plus child headings and output, decrementing one shared five-painted-row budget for every wrapped output row in command order. Direct clicking of a visible Shell heading remains a full individual expansion. Pass the selected mode through normal, fullscreen, overlay, permanent, and rewrap paths; mode changes invalidate cached wrapping and preserve existing scroll-back behavior.

- [ ] **Step 4: Verify the tests pass**

Run: `cargo test hidden_shell_group_paints_no_rows expanded_shell_group_caps_output_at_five_painted_rows_across_children collapsed_shell_heading_ellipsizes_instead_of_wrapping`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add src/renderer.rs
git commit -m "feat: render shell display modes"
```

### Task 3: Retain final completion at the Shell's initial position

**Files:**
- Modify: `src/state.rs: ShellBatch, start_item, complete_shell_batch_member, tests`
- Modify: `src/renderer.rs: fullscreen committed-block replacement, tests`

**Interfaces:**
- `ShellBatch` produces a stable running `anchor: Block`.
- Final `shell_results_block` adopts `anchor.id()`.
- Fullscreen renderer replaces an existing history block with the same id.

- [ ] **Step 1: Write failing tests**

```rust
let anchor_id = state.drain_committed().single().id();
complete_shell(&mut state, "shell-1", 0, "done");
assert_eq!(state.drain_committed().single().id(), anchor_id);
assert_eq!(replace_history_block(vec![anchor], completed)[0].title, "Shell · 1 command · completed");
```

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test completed_shell_group_reuses_its_running_anchor_id fullscreen_replaces_an_anchored_shell_instead_of_appending_it`

Expected: compile failure because stable anchors and replacement are absent.

- [ ] **Step 3: Add stable anchors and replacement**

When a Shell batch receives its first member, enqueue one running anchor and save it on the batch; do not duplicate that batch in live output. When the final member completes, build the final summary, adopt the anchor id, and enqueue it. In fullscreen, replace matching history by id and rewrap rather than append. Inline terminals cannot rewrite scrollback, so they print the final completed replacement when it arrives.

- [ ] **Step 4: Verify the tests pass**

Run: `cargo test completed_shell_group_reuses_its_running_anchor_id fullscreen_replaces_an_anchored_shell_instead_of_appending_it`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add src/state.rs src/renderer.rs
git commit -m "fix: retain shell completion position"
```

### Task 4: Verify the completed feature

**Files:**
- Verify: `src/renderer.rs`, `src/state.rs`, `src/main.rs`

- [ ] **Step 1: Run formatting and focused tests**

Run: `cargo fmt --check; cargo test renderer::tests::; cargo test state::tests::; cargo test main::tests::`

Expected: every command exits 0.

- [ ] **Step 2: Run final regression checks**

Run: `cargo test --quiet; git diff --check; git status --short`

Expected: all tests pass, no whitespace errors, and no unintended files remain.
