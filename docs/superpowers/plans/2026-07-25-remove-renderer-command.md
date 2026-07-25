# Remove Renderer Slash Command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the `/renderer` slash command without removing startup renderer configuration.

**Architecture:** Delete the command at its catalog and dispatch boundaries, then remove the action and persistence writer that become unreachable. Keep `RenderMode`, `load_render_mode`, CLI parsing, environment parsing, and saved preference reading intact.

**Tech Stack:** Rust 2024, crossterm 0.29, existing unit tests in `src/state.rs` and `src/renderer.rs`.

## Global Constraints

- `/renderer` must not appear in slash suggestions.
- `/renderer` and `/renderer <mode>` must use the generic unknown-command path.
- `--renderer`, `DEVEZ_RENDERER`, and existing `renderer.txt` loading remain unchanged.
- Fullscreen and inline rendering implementations remain unchanged.
- Preserve unrelated working-tree edits.

---

### Task 1: Remove the Renderer Command and Dead Dispatch

**Files:**
- Modify: `src/state.rs:85-115`
- Modify: `src/state.rs:520-545`
- Modify: `src/state.rs:3735-3780`
- Modify: `src/state.rs:5235-5270`
- Modify: `src/main.rs:410-430`
- Modify: `src/main.rs:770-790`
- Modify: `src/main.rs:1825-1845`
- Modify: `src/renderer.rs:55-90`
- Test: `src/state.rs` test module

**Interfaces:**
- Preserves: `RenderMode::parse(value: &str) -> Option<RenderMode>`
- Preserves: `load_render_mode(cli_override: Option<&str>) -> Result<RenderMode>`
- Removes: `Action::SetRenderMode(RenderMode)`
- Removes: `AppState::apply_render_mode(RenderMode) -> Action`
- Removes: `save_render_mode(RenderMode) -> Result<()>`

- [ ] **Step 1: Replace the renderer-command test with the desired behavior**

```rust
#[test]
fn renderer_is_not_a_slash_command() {
    let mut state = test_state();

    state.editor.set_text("/rend");
    assert!(state.matching_slash_commands().is_empty());

    assert!(matches!(
        state.run_slash_command("/renderer inline"),
        Action::None
    ));
    let error = state.committed.last().expect("unknown-command error");
    assert_eq!(error.title, "알 수 없는 명령");
    assert!(error.body.contains("/renderer inline"));
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```powershell
cargo test renderer_is_not_a_slash_command
```

Expected: FAIL because `/renderer` is still suggested or dispatches
`Action::SetRenderMode`.

- [ ] **Step 3: Remove command registration and state dispatch**

Delete the `/renderer` entry from `SLASH_COMMANDS` and reduce the array length
from 25 to 24. Delete both `/renderer` match arms, `Action::SetRenderMode`, and
`apply_render_mode`. Remove `RenderMode` from `state.rs` imports once unused.

- [ ] **Step 4: Remove unreachable main-loop and persistence code**

Delete `Action::SetRenderMode(_)` from local-action match groups and remove its
execution arm. Delete `renderer::save_render_mode`; keep `render_mode_file`
because `load_render_mode` still reads the existing preference.

- [ ] **Step 5: Run focused compatibility tests**

Run:

```powershell
cargo test renderer_is_not_a_slash_command
cargo test render_mode_parses_its_aliases_and_rejects_the_rest
```

Expected: both pass.

- [ ] **Step 6: Run complete verification**

Run:

```powershell
cargo test
cargo build
cargo fmt --check
git diff --check -- src/main.rs src/renderer.rs src/state.rs
```

Expected: all commands exit 0 without compiler warnings.

- [ ] **Step 7: Preserve the mixed working tree**

Do not create a source commit because the three modified files already contain
unrelated user changes. Report the verified working-tree result.

