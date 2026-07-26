# Effort Compute Steps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Claude-like `/effort` axis and marker with Devez's compact, responsive Compute Steps row without changing selection behavior.

**Architecture:** Keep `EffortSlider` as the state-to-renderer interface and change only the renderer. Build a single row of styled `PaintSpan`s, choose full or compact labels from the available width, then center the row inside the existing picker frame.

**Tech Stack:** Rust 2024, Crossterm, `unicode-width`, built-in Rust unit tests

## Global Constraints

- Keep `EffortSlider`, effort ordering, supported-effort filtering, and keyboard handling unchanged.
- Render only effort values supplied by the server.
- Use the existing `Tone::Effort*` palette and `Tone::Accent` fallback.
- Keep the selected label fully spelled and uppercase at every width.
- Do not add scrolling, wrapping, dependencies, or unrelated renderer refactors.
- `src/renderer.rs` already has unrelated uncommitted edits; do not stage or commit the whole file.

---

### Task 1: Replace the effort axis with responsive Compute Steps

**Files:**
- Modify: `src/renderer.rs:1325-1418`
- Test: `src/renderer.rs:4597-4638`

**Interfaces:**
- Consumes: `EffortSlider { efforts: Vec<String>, selected: usize }`, `panel_span(width: u16) -> usize`, and `effort_tone(effort: &str) -> Option<Tone>`.
- Produces: `effort_step_lines(slider: &EffortSlider, width: u16) -> Vec<PaintLine>`.
- Produces: `effort_step_spans(slider: &EffortSlider, selected: usize, compact: bool) -> Vec<PaintSpan>`.
- Produces: `effort_step_label(effort: &str, selected: bool, compact: bool) -> String`.

- [ ] **Step 1: Replace the old renderer test with failing Compute Steps tests**

Replace `the_effort_track_is_centred_and_coloured_by_tier` with:

```rust
#[test]
fn effort_steps_replace_the_axis_with_one_coloured_row() {
    let slider = EffortSlider {
        efforts: ["low", "medium", "high", "xhigh", "max", "ultra"]
            .map(ToOwned::to_owned)
            .to_vec(),
        selected: 2,
    };

    let lines = effort_step_lines(&slider, 100);

    assert_eq!(lines.len(), 2);
    let steps = &lines[1];
    assert_eq!(
        painted(steps).trim(),
        "low › medium › HIGH › xhigh › max › ultra"
    );
    assert_eq!(steps.prefix, " ".repeat(29));
    let text = painted(steps);
    assert!(!text.contains("Faster"));
    assert!(!text.contains("Smarter"));
    assert!(!text.contains('▲'));
    assert!(!text.contains('─'));

    let selected = steps
        .tail
        .iter()
        .position(|span| span.text == "HIGH")
        .expect("selected effort");
    assert_eq!(steps.tail[selected].tone, Tone::EffortHigh);
    assert!(steps.tail[selected].bold);
    assert_eq!(steps.tail[selected + 1].text, " › ");
    assert_eq!(steps.tail[selected + 1].tone, Tone::EffortHigh);
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

    assert_eq!(painted(&lines[1]).trim(), "L › M › HIGH › XH › MAX › U");
}

#[test]
fn effort_steps_handle_empty_efforts_and_a_stale_selection() {
    assert!(effort_step_lines(
        &EffortSlider {
            efforts: Vec::new(),
            selected: 0,
        },
        80,
    )
    .is_empty());

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

    assert_eq!(selected.text, "ULTRA");
    assert_eq!(selected.tone, Tone::EffortUltra);
}
```

- [ ] **Step 2: Run the focused tests and verify they fail for the missing renderer**

Run:

```powershell
cargo test effort_steps -- --nocapture
```

Expected: compilation fails because `effort_step_lines` does not exist yet.

- [ ] **Step 3: Implement the minimal Compute Steps renderer**

In `overlay_frame`, replace:

```rust
lines.extend(effort_slider_lines(&slider, width));
```

with:

```rust
lines.extend(effort_step_lines(&slider, width));
```

Replace `EFFORT_SLOT_WIDTH`, `effort_slider_lines`, `selected_effort_tone`, and
`center_cell` with:

```rust
const EFFORT_SEPARATOR: &str = " › ";

fn effort_step_lines(slider: &EffortSlider, width: u16) -> Vec<PaintLine> {
    if slider.efforts.is_empty() {
        return Vec::new();
    }

    let selected = slider.selected.min(slider.efforts.len() - 1);
    let inner = panel_span(width);
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

    vec![
        PaintLine::blank(),
        PaintLine {
            prefix: " ".repeat(inner.saturating_sub(content_width) / 2),
            prefix_tone: Tone::Muted,
            text: String::new(),
            tone: Tone::Muted,
            bold: false,
            tail: spans,
        },
    ]
}

fn effort_step_spans(
    slider: &EffortSlider,
    selected: usize,
    compact: bool,
) -> Vec<PaintSpan> {
    let selected_tone = slider
        .efforts
        .get(selected)
        .and_then(|effort| effort_tone(effort))
        .unwrap_or(Tone::Accent);
    let mut spans = Vec::with_capacity(slider.efforts.len() * 2 - 1);

    for (index, effort) in slider.efforts.iter().enumerate() {
        if index > 0 {
            spans.push(PaintSpan {
                text: EFFORT_SEPARATOR.to_owned(),
                tone: if index == selected + 1 {
                    selected_tone
                } else {
                    Tone::Muted
                },
                bold: false,
            });
        }

        let is_selected = index == selected;
        spans.push(PaintSpan {
            text: effort_step_label(effort, is_selected, compact),
            tone: if is_selected {
                selected_tone
            } else {
                Tone::Muted
            },
            bold: is_selected,
        });
    }

    spans
}

fn effort_step_label(effort: &str, selected: bool, compact: bool) -> String {
    if selected {
        return effort.to_ascii_uppercase();
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
```

- [ ] **Step 4: Run the focused tests and verify they pass**

Run:

```powershell
cargo test effort_steps -- --nocapture
```

Expected: all three `effort_steps_*` tests pass.

- [ ] **Step 5: Verify formatting and the full test suite**

Run:

```powershell
cargo fmt -- --check
cargo test
```

Expected: formatting check exits `0`; the full test suite reports no failures.

- [ ] **Step 6: Review only the feature diff**

Run:

```powershell
git diff -- src/renderer.rs
git status --short
```

Verify that the new edits are limited to the effort renderer, its call site, and
the three renderer tests. Because `src/renderer.rs` had unrelated edits before
this task, leave the implementation uncommitted unless those earlier edits have
been separated by the user.
