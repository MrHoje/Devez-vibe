# Keyboard-only Prompt History Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent mouse-wheel movement from recalling prompt history while preserving keyboard Up and Down history navigation.

**Architecture:** Fullscreen terminal setup will save and disable DEC private mode 1007 so the terminal cannot translate wheel movement into arrow-key input. Fullscreen teardown will restore the saved mode; application key handling and inline rendering remain unchanged.

**Tech Stack:** Rust, Crossterm 0.29, built-in Rust unit tests

## Global Constraints

- Change only fullscreen terminal setup and teardown in `src/renderer.rs`.
- Do not enable mouse capture.
- Keep `AppState` and `Editor` keyboard history handling unchanged.
- Preserve inline renderer scrolling and Shift+arrow transcript navigation.
- Preserve unrelated working-tree changes.

---

### Task 1: Disable alternate-scroll translation in fullscreen mode

**Files:**
- Modify: `src/renderer.rs:227-260`
- Test: `src/renderer.rs:3226-3233`

**Interfaces:**
- Consumes: Crossterm's `EnterAlternateScreen`, `LeaveAlternateScreen`, `Print`, and `execute!`.
- Produces: `enter_fullscreen(out: &mut impl Write) -> std::io::Result<()>` and `leave_fullscreen(out: &mut impl Write) -> std::io::Result<()>`.

- [ ] **Step 1: Write the failing entry and exit tests**

Replace the current fullscreen-entry assertion and add an exit assertion:

```rust
#[test]
fn fullscreen_disables_wheel_to_arrow_translation_without_mouse_capture() {
    let mut output = Vec::new();

    enter_fullscreen(&mut output).expect("fullscreen command");

    assert_eq!(
        output,
        b"\x1b[?1049h\x1b[?1007s\x1b[?1007l",
        "enter alternate screen, save alternate-scroll mode, then disable it"
    );
}

#[test]
fn fullscreen_exit_restores_alternate_scroll_before_leaving() {
    let mut output = Vec::new();

    leave_fullscreen(&mut output).expect("fullscreen exit command");

    assert_eq!(output, b"\x1b[?1007r\x1b[?1049l");
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```powershell
cargo test renderer::tests::fullscreen_ -- --nocapture
```

Expected: compilation fails because `leave_fullscreen` does not exist, and the updated entry expectation cannot be satisfied by the existing `enter_fullscreen`.

- [ ] **Step 3: Implement the minimal terminal-mode change**

Update the two fullscreen helpers:

```rust
fn enter_fullscreen(out: &mut impl Write) -> std::io::Result<()> {
    // Mode 1007 turns wheel movement into Up/Down key sequences on the alternate
    // screen. Save the user's setting, then disable only that translation.
    execute!(
        out,
        EnterAlternateScreen,
        Print("\x1b[?1007s\x1b[?1007l")
    )
}

fn leave_fullscreen(out: &mut impl Write) -> std::io::Result<()> {
    execute!(out, Print("\x1b[?1007r"), LeaveAlternateScreen)
}
```

In `TerminalSession::drop`, replace:

```rust
let _ = execute!(stdout(), LeaveAlternateScreen);
```

with:

```rust
let _ = leave_fullscreen(&mut stdout());
```

- [ ] **Step 4: Run focused and full verification**

Run:

```powershell
cargo test renderer::tests::fullscreen_ -- --nocapture
cargo test
cargo fmt -- --check
```

Expected: both focused tests pass, the full test suite passes, and formatting reports no differences.

- [ ] **Step 5: Review the scoped diff**

Run:

```powershell
git diff --check -- src/renderer.rs
git diff -- src/renderer.rs
```

Expected: only the fullscreen helpers, teardown call, and two focused tests differ for this change; pre-existing user edits remain intact.
