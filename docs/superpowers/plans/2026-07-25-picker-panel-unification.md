# Picker Panel Unification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render every common Picker with the same closed titled panel as Commands, remove duplicate Model/Effort copy, and leave one blank row before the statusline whenever an overlay replaces the normal prompt.

**Architecture:** Keep interaction state and `OverlayView` unchanged. Normalize picker copy in `src/state.rs`, then make the `OverlayStyle::Picker` renderer reuse the Commands title, side-border, padding, and bottom-border primitives in `src/renderer.rs`; add the statusline gap once in the common overlay frame.

**Tech Stack:** Rust, Ratatui-style terminal paint rows, `unicode-width`, built-in Rust unit tests.

## Global Constraints

- Common `Picker` overlays must use a closed Commands-style border.
- `/effort` must not show the current model name in its body.
- `/model` must not show a standalone `Effort` body heading.
- The selected effort's existing small closed border must remain.
- Every overlay that hides the normal prompt must have exactly one blank row immediately before the statusline.
- Selection, confirmation, cancellation, and direct slash-command behavior must not change.
- Add no dependencies.
- The worktree already contains unrelated edits; stage only the hunks created by each task.

---

### Task 1: Remove redundant picker copy from overlay state

**Files:**
- Modify: `src/state.rs:4421-4549`
- Test: `src/state.rs:6836-7040`

**Interfaces:**
- Consumes: existing `AppState::overlay_view() -> Option<OverlayView<'_>>`
- Produces: Model title `"Model"`, Effort title `"Effort"` with no body rows, Theme title `"Theme"`, and a Model slider preceded only by its existing blank separator

- [ ] **Step 1: Write the failing state tests**

Add this test beside the existing model and effort picker tests:

```rust
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
```

Extend `theme_command_supports_picker_and_direct_selection` immediately after obtaining its overlay:

```rust
assert_eq!(overlay.title, "Theme");
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```powershell
cargo test model_and_effort_picker_copy_is_not_duplicated_in_the_body
cargo test theme_command_supports_picker_and_direct_selection
```

Expected: the first test fails on `"Select model"` versus `"Model"`; the theme test fails on `"Select theme"` versus `"Theme"`.

- [ ] **Step 3: Make the minimal state changes**

In the `PendingInteraction::ModelPicker` branch:

```rust
let slider = self.models.get(*model_index).map(|model| {
    lines.push(OverlayLine {
        text: String::new(),
        selected: false,
        muted: true,
    });
    effort_slider(model, *effort_index)
});
Some(OverlayView {
    title: "Model".to_owned(),
    lines,
    slider,
    hint: "1-9 select  ·  ↑↓ model  ·  ←→ effort  ·  Enter to continue  ·  Esc to cancel"
        .to_owned(),
    style: OverlayStyle::Picker,
    input: None,
    input_label: "",
    input_placeholder: "",
})
```

Replace the Effort branch body with:

```rust
PendingInteraction::EffortPicker { effort_index } => {
    let model = self.selected_model()?;
    Some(OverlayView {
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
```

Change the Theme branch title only:

```rust
title: "Theme".to_owned(),
```

- [ ] **Step 4: Run the state tests**

Run:

```powershell
cargo test model_and_effort_picker_copy_is_not_duplicated_in_the_body
cargo test theme_command_supports_picker_and_direct_selection
cargo test model_aliases_and_number_keys_select_catalog_entries
```

Expected: all commands report `ok`.

- [ ] **Step 5: Review and commit only Task 1 hunks**

Run:

```powershell
git diff -- src/state.rs
git add -p -- src/state.rs
git diff --cached --check
git diff --cached -- src/state.rs
git commit -m "refactor: simplify picker labels"
```

Confirm the staged diff contains only the three title/body changes and their tests before committing.

---

### Task 2: Render Pickers as closed panels and separate overlays from statusline

**Files:**
- Modify: `src/renderer.rs:1336-1508`
- Modify: `src/renderer.rs:1577-1655`
- Modify: `src/renderer.rs:1729-1876`
- Test: `src/renderer.rs:5370-5610`

**Interfaces:**
- Consumes: `OverlayView`, `panel_span`, `panel_padding_row`, `panel_line_keep_left`, `panel_bottom`, `close_panel_row`, and `effort_step_lines`
- Produces: `panel_title_row(title: &str, panel_width: usize) -> PaintLine` and `panelize_content_line(line: PaintLine, panel_width: usize) -> PaintLine`

- [ ] **Step 1: Replace the borderless-picker test with a failing closed-panel test**

Replace `picker_overlay_uses_restrained_borderless_chrome` with:

```rust
#[test]
fn picker_overlay_matches_the_commands_closed_panel() {
    let frame = overlay_frame(
        &[],
        OverlayView {
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
            fallback: "status".to_owned(),
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
        panel.iter().all(|line| painted_width(line) == panel_span(80)),
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
```

- [ ] **Step 2: Add failing statusline-gap coverage**

Add this test beside the overlay frame tests:

```rust
#[test]
fn every_overlay_keeps_exactly_one_blank_row_before_the_statusline() {
    for style in [OverlayStyle::Picker, OverlayStyle::Panel] {
        let frame = overlay_frame(
            &[],
            OverlayView {
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
```

Extend `a_picker_with_a_search_field_keeps_a_gap_above_the_composer` so an overlay-specific input also verifies the new status gap:

```rust
let status = frame.lines.len() - 1;
assert!(
    painted(&frame.lines[status - 1]).trim().is_empty(),
    "the overlay input runs straight into the statusline"
);
assert!(
    !painted(&frame.lines[status - 2]).trim().is_empty(),
    "there must be exactly one blank row before the statusline"
);
```

- [ ] **Step 3: Run the renderer tests to verify they fail**

Run:

```powershell
cargo test picker_overlay_matches_the_commands_closed_panel
cargo test every_overlay_keeps_exactly_one_blank_row_before_the_statusline
```

Expected: the first test fails because Picker still has open horizontal rules; the second fails because the row before the statusline is not blank.

- [ ] **Step 4: Extract the existing Commands title row**

Add this helper near `panel_rule_row`:

```rust
fn panel_title_row(title: &str, panel_width: usize) -> PaintLine {
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
        tail: vec![PaintSpan {
            text: format!("{}╮", "─".repeat(header_rule)),
            tone: Tone::Border,
            bold: false,
        }],
    }
}
```

Replace the hand-built header at the start of `suggestion_lines` with:

```rust
let title = suggestions
    .first()
    .map(|suggestion| suggestion.panel_title)
    .unwrap_or("Commands");
let mut lines = vec![panel_title_row(title, panel_width)];
```

This keeps the Commands output unchanged while giving Picker the exact same title row.

- [ ] **Step 5: Add a side-border adapter for effort paint rows**

Add beside `close_panel_row`:

```rust
fn panelize_content_line(mut line: PaintLine, panel_width: usize) -> PaintLine {
    line.prefix.insert(0, '│');
    line.prefix_tone = Tone::Border;
    close_panel_row(line, panel_width)
}
```

Split the effort renderer so Picker can pass its exact inner width without changing existing call semantics:

```rust
fn effort_step_lines(slider: &EffortSlider, width: u16) -> Vec<PaintLine> {
    effort_step_lines_in(slider, panel_span(width))
}

fn effort_step_lines_in(slider: &EffortSlider, inner: usize) -> Vec<PaintLine> {
    if slider.efforts.is_empty() {
        return Vec::new();
    }

    let selected = slider.selected.min(slider.efforts.len() - 1);
    let full = effort_step_spans(slider, selected, false);
    let full_width = full
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum::<usize>();
    let spans = if full_width <= inner {
        full
    } else {
        effort_step_spans(slider, selected, true)
    };
    let content_width = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum::<usize>();
    let indent = inner.saturating_sub(content_width) / 2;
    let selected_span = spans
        .iter()
        .position(|span| span.bold)
        .expect("non-empty effort list has a selection");
    let selected_offset = spans[..selected_span]
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum::<usize>();
    let selected_width = UnicodeWidthStr::width(spans[selected_span].text.as_str());
    let selected_tone = spans[selected_span].tone;
    let border_prefix = " ".repeat(indent + selected_offset);
    let border_fill = "─".repeat(selected_width.saturating_sub(2));

    vec![
        PaintLine::blank(),
        PaintLine {
            prefix: border_prefix.clone(),
            prefix_tone: Tone::Muted,
            text: format!("╭{border_fill}╮"),
            tone: selected_tone,
            bold: true,
            tool_heading: None,
            tail: Vec::new(),
        },
        PaintLine {
            prefix: " ".repeat(indent),
            prefix_tone: Tone::Muted,
            text: String::new(),
            tone: Tone::Muted,
            bold: false,
            tool_heading: None,
            tail: spans,
        },
        PaintLine {
            prefix: border_prefix,
            prefix_tone: Tone::Muted,
            text: format!("╰{border_fill}╯"),
            tone: selected_tone,
            bold: true,
            tool_heading: None,
            tail: Vec::new(),
        },
    ]
}
```

Replace the current `effort_step_lines` with both functions above.

- [ ] **Step 6: Replace the Picker rendering branch**

Delete the now-unused `picker_rule` function. Replace `OverlayStyle::Picker => { ... }` with:

```rust
OverlayStyle::Picker => {
    let panel_width = panel_span(width);
    let inner_width = panel_width.saturating_sub(2);
    lines.push(panel_title_row(&overlay.title, panel_width));
    lines.push(panel_padding_row(panel_width));

    for row in overlay.lines {
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
                (panel_width.saturating_sub(1)).min(u16::MAX as usize) as u16,
            );
            lines.extend(
                wrapped
                    .into_iter()
                    .map(|line| close_panel_row(line, panel_width)),
            );
        }
    }

    if let Some(slider) = overlay.slider {
        lines.extend(
            effort_step_lines_in(&slider, inner_width)
                .into_iter()
                .map(|line| panelize_content_line(line, panel_width)),
        );
    }
    lines.push(panel_line_keep_left(
        &format!("   {}", overlay.hint),
        panel_width,
        Tone::Muted,
        false,
    ));
    lines.push(panel_padding_row(panel_width));
    lines.push(panel_bottom(inner_width));
}
```

- [ ] **Step 7: Add the common statusline gap**

Immediately before `status_line_row` is pushed in `overlay_frame_with_expansion`, insert:

```rust
lines.push(PaintLine::blank());
lines.push(status_line_row(status.line, &status.fallback, width));
```

Keep the existing blank row between a panel and `overlay.input`; this new row has a different purpose and sits between the final overlay/input row and statusline.

- [ ] **Step 8: Run focused renderer tests**

Run:

```powershell
cargo test picker_overlay_matches_the_commands_closed_panel
cargo test every_overlay_keeps_exactly_one_blank_row_before_the_statusline
cargo test a_picker_with_a_search_field_keeps_a_gap_above_the_composer
cargo test effort_steps_
cargo test suggestion_
```

Expected: all commands report `ok`.

- [ ] **Step 9: Format and run the full test suite**

Run:

```powershell
cargo fmt --check
cargo test
```

Expected: formatting succeeds and all tests pass.

- [ ] **Step 10: Review and commit only Task 2 hunks**

Run:

```powershell
git diff -- src/renderer.rs
git add -p -- src/renderer.rs
git diff --cached --check
git diff --cached -- src/renderer.rs
git commit -m "feat: unify picker panel chrome"
```

Confirm the staged diff contains only the shared title helper, Picker panel renderer, effort-row adapter, statusline gap, and related tests before committing.
