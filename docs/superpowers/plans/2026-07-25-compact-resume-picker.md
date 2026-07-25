# Compact Resume Picker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show `/resume` sessions in a command-style, single-line dock with at most 10 visible rows.

**Architecture:** Add one renderer-owned compact overlay style that truncates rows instead of wrapping them. Keep filtering and selection in `SessionPicker`, which formats time-first rows and uses its own 10-row window; both runtime and startup resume flows already consume this shared picker.

**Tech Stack:** Rust, crossterm terminal rendering, built-in unit tests

## Global Constraints

- Apply the same picker to runtime `/resume` and startup `dvz --resume`.
- Preserve search and all existing picker key bindings.
- Keep existing `Panel` and `Picker` overlay rendering unchanged.
- Add no dependencies.

---

### Task 1: Compact Overlay Rendering

**Files:**
- Modify: `src/renderer.rs:169-173`
- Modify: `src/renderer.rs:1729-1800`
- Test: `src/renderer.rs:5004-5245`

**Interfaces:**
- Consumes: existing `OverlayView`, `OverlayLine`, `panel_line_keep_left`, and `panel_rule_row`
- Produces: `OverlayStyle::CompactPanel`, rendered as a bordered, right-truncated, one-row-per-option dock

- [ ] **Step 1: Write the failing renderer test**

Add this test beside the other overlay tests:

```rust
#[test]
fn compact_panel_keeps_each_option_on_one_physical_row() {
    let live = [Block::new(BlockKind::Assistant, "Codex", "existing reply")];
    let frame = overlay_frame(
        &live,
        OverlayView {
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
    assert!(frame.lines.iter().any(|line| painted(line).contains("existing reply")));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test renderer::tests::compact_panel_keeps_each_option_on_one_physical_row -- --nocapture`

Expected: FAIL because `OverlayStyle::CompactPanel` does not exist.

- [ ] **Step 3: Implement the compact overlay style**

Extend the enum:

```rust
pub enum OverlayStyle {
    Panel,
    CompactPanel,
    Picker,
}
```

Add a dedicated `CompactPanel` arm in `overlay_frame_with_expansion`. Use the
same top/padding/bottom structure as `Panel`, but render each `OverlayLine`
exactly once:

```rust
OverlayStyle::CompactPanel => {
    let panel_width = panel_span(width);
    lines.push(panel_rule_row("╭─ ", &overlay.title, '╮', panel_width));
    lines.push(panel_padding_row(panel_width));
    for row in overlay.lines {
        let marker = if row.selected { "❯" } else { " " };
        lines.push(panel_line_keep_left(
            &format!(" {marker} {}", row.text),
            panel_width,
            if row.selected {
                Tone::Accent
            } else if row.muted {
                Tone::Muted
            } else {
                Tone::Plain
            },
            row.selected,
        ));
    }
    lines.push(panel_padding_row(panel_width));
    lines.push(panel_rule_row("╰─ ", &overlay.hint, '╯', panel_width));
}
```

`panel_line_keep_left` already replaces control characters and truncates the
right side, so newline-containing or overlong input remains one physical row.

- [ ] **Step 4: Run focused renderer tests**

Run: `cargo test renderer::tests::compact_panel_keeps_each_option_on_one_physical_row -- --nocapture`

Expected: PASS.

Run: `cargo test renderer::tests::panel_overlay_keeps_its_border_when_a_row_folds -- --nocapture`

Expected: PASS, confirming the existing panel renderer is unchanged.

- [ ] **Step 5: Commit**

```powershell
git add -- src/renderer.rs
git commit -m "feat: add compact overlay panel"
```

### Task 2: Time-First 10-Row Resume Picker

**Files:**
- Modify: `src/state.rs:1822-2005`
- Test: `src/state.rs:6889-6916`

**Interfaces:**
- Consumes: `OverlayStyle::CompactPanel` from Task 1 and existing `visible_window`
- Produces: `const RESUME_PICKER_ROWS: usize = 10` and compact time-first session rows from `SessionPicker::overlay_view`

- [ ] **Step 1: Write failing state tests**

Replace the existing resume-picker layout test with:

```rust
#[test]
fn resume_picker_shows_ten_time_first_single_line_rows() {
    let sessions = (0..12)
        .map(|index| SessionInfo {
            id: format!("session-{index}"),
            name: Some(format!("Session {index}")),
            preview: String::new(),
            cwd: format!(r"C:\work\project-{index}"),
            updated_at: 0,
        })
        .collect();
    let mut picker = SessionPicker::new(sessions, r"C:\work\project-0".to_owned(), None);
    picker.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));

    let view = picker.overlay_view();

    assert!(matches!(view.style, OverlayStyle::CompactPanel));
    assert_eq!(view.lines.len(), 10);
    assert!(view.lines[0].text.starts_with("unknown"));
    assert!(view.lines[0].text.contains("Session 0"));
    assert!(view.lines[0].text.contains(r"C:\work\project-0"));
    assert!(view.lines.iter().all(|line| !line.text.contains('\n')));
    assert!(view.input_label.is_empty());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test state::tests::resume_picker_shows_ten_time_first_single_line_rows -- --nocapture`

Expected: FAIL because the picker still uses nine rows, title-first formatting, and `OverlayStyle::Panel`.

- [ ] **Step 3: Implement the resume row model**

Add a picker-local row limit near `SessionPicker`:

```rust
const RESUME_PICKER_ROWS: usize = 10;
```

In `SessionPicker::overlay_view`, use
`visible_window(Some(self.selected), filtered.len(), RESUME_PICKER_ROWS)`.
Replace the title-first/multiline path formatting with:

```rust
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
```

Set `style: OverlayStyle::CompactPanel`. Keep the title, search editor,
placeholder, hint, filtering, selection, and key handling unchanged.

- [ ] **Step 4: Run focused state and renderer tests**

Run: `cargo test resume_picker -- --nocapture`

Expected: all resume-picker tests PASS.

Run: `cargo test renderer::tests::compact_panel -- --nocapture`

Expected: compact overlay tests PASS.

- [ ] **Step 5: Run full verification**

Run: `cargo fmt --check`

Expected: exit code 0.

Run: `cargo test`

Expected: all tests PASS.

- [ ] **Step 6: Commit**

```powershell
git add -- src/state.rs
git commit -m "feat: compact resume session picker"
```
