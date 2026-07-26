# Welcome Fixed Left Column Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep welcome information at 48 cells and allocate every remaining split-panel cell to release notes.

**Architecture:** Change the column-width calculation in `welcome_lines`; the existing row builders and wrapping remain consumers of their assigned widths. A focused renderer unit test asserts the visible split border geometry.

**Tech Stack:** Rust, Cargo test framework, Ratatui-style terminal renderer.

## Global Constraints

- Preserve the existing one-column behavior below an inner width of 62 cells.
- Do not change content, tones, borders, or release-note wrapping behavior.

---

### Task 1: Fixed-width split geometry

**Files:**
- Modify: `src/renderer.rs:1978-2019`
- Test: `src/renderer.rs:8611-8631`

**Interfaces:**
- Consumes: `welcome_lines(welcome: WelcomeView, width: u16) -> Vec<PaintLine>`
- Produces: split panels with a 48-cell left column and a right column of `inner_width - 49` cells.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn wide_welcome_panel_keeps_info_column_at_48_cells() {
    let lines = welcome_lines(test_welcome(), 110);
    let top = painted(&lines[0]);
    let (left, right) = top.trim_matches(['╭', '╮']).split_once('┬').expect("split border");
    assert_eq!(left.chars().count(), 48);
    assert_eq!(right.chars().count(), panel_span(110) - 2 - 48 - 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test wide_welcome_panel_keeps_info_column_at_48_cells`

Expected: FAIL because the current proportional left column is 64 cells at width 110.

- [ ] **Step 3: Write minimal implementation**

```rust
const WELCOME_INFO_WIDTH: usize = 48;
let left_width = WELCOME_INFO_WIDTH;
let right_width = inner_width - left_width - 1;
```

- [ ] **Step 4: Run focused tests to verify they pass**

Run: `cargo test welcome_panel`

Expected: PASS, including wide split geometry and narrow single-column behavior.

- [ ] **Step 5: Commit the scoped change**

```bash
git add src/renderer.rs
git commit -m "fix: let welcome notes column expand"
```
