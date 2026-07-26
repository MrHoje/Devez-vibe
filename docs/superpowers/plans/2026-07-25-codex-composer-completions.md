# Codex Composer Completions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add current-Codex `$` and `@` composer completion semantics with DevezCLI-native rendering.

**Architecture:** A focused completion module supplies cursor-aware targets,
ranked candidates, workspace entries, and insertion values. `AppState` owns
selection/mode/dismissal state and maps existing integration catalogs into that
model; the renderer remains a presentation-only dock.

**Tech Stack:** Rust 2024, crossterm, serde_json, `ignore`, existing unit-test modules.

## Global Constraints

- Preserve all unrelated dirty-worktree changes.
- Keep DevezCLI's docked panel, border, and palette style.
- `$` contains Plugin, Skill, and App; `@` contains Plugin, Skill, File, and Dir.
- Skill, plugin, app, and filesystem insertion strings match current Codex.
- Do not block completion when filesystem indexing fails.

---

### Task 1: Pure completion model

**Files:**
- Create: `src/completion.rs`
- Modify: `src/main.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces: `CompletionTarget`, `CompletionCandidate`, `CompletionKind`,
  `CompletionMode`, `completion_target`, `filter_candidates`,
  `collect_workspace_entries`, and `completion_text`.

- [ ] **Step 1: Write failing tests**

Add tests proving:

```rust
assert_eq!(completion_target("@src/main", 9).unwrap().query, "src/main");
assert!(completion_target("dev@example.com", 15).is_none());
assert!(completion_target("$HOME", 5).is_none());
assert_eq!(completion_text(CompletionKind::Skill, "review", None), "$review");
assert_eq!(completion_text(CompletionKind::File, "src/main.rs", None), "src/main.rs");
```

- [ ] **Step 2: Verify RED**

Run: `cargo test completion::tests -- --nocapture`

Expected: FAIL because `completion` does not exist.

- [ ] **Step 3: Implement the minimal model**

Implement char-indexed token ranges, case-insensitive fuzzy subsequence ranking,
Codex category ordering, mode filtering, ignore-aware relative workspace
entries, and insertion text/quoting.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test completion::tests -- --nocapture`

Expected: PASS.

### Task 2: State catalog and keyboard integration

**Files:**
- Modify: `src/editor.rs`
- Modify: `src/state.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: all Task 1 completion interfaces.
- Produces: `AppState::update_workspace_entries(Vec<WorkspaceEntry>)`,
  completion-aware `AppState::handle_key`, and completion panel data in `View`.

- [ ] **Step 1: Write failing tests**

Add tests proving:

```rust
// `$` exposes plugin, skill, and app categories.
// `@` exposes plugin, skill, file, and directory categories.
// Enter inserts the selected value and does not submit the prompt.
// A selected skill from `@` becomes `$skill-name `.
// A selected file replaces only the active mid-draft `@query`.
// Esc dismisses the unchanged token; editing allows it to reopen.
```

Add an editor test proving a char range can be replaced without losing the
surrounding multiline draft.

- [ ] **Step 2: Verify RED**

Run: `cargo test composer_completion -- --nocapture`

Run: `cargo test replace_range -- --nocapture`

Expected: FAIL because completion state and range replacement are missing.

- [ ] **Step 3: Implement minimal state integration**

Map enabled integration bindings into candidates, route popup keys before
history/navigation keys, maintain selection/mode/dismissal state, replace the
active range, and append or consume exactly one separator.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test state::tests editor::tests -- --nocapture`

Expected: PASS.

### Task 3: DevezCLI completion dock

**Files:**
- Modify: `src/renderer.rs`
- Modify: `src/state.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes: completion heading, category, selection, description, and hint from
  Task 2.
- Produces: the final DevezCLI-styled completion panel.

- [ ] **Step 1: Write failing renderer tests**

Assert that the panel:

```rust
// uses the dynamic "Tools" or "Mentions" heading,
// prints [Skill]/[Plugin]/[App]/[File]/[Dir] category labels,
// keeps the selected row visible,
// renders the `@` search-mode hint within the existing bordered dock.
```

- [ ] **Step 2: Verify RED**

Run: `cargo test renderer::tests::completion -- --nocapture`

Expected: FAIL because the dock supports command-only content.

- [ ] **Step 3: Implement minimal rendering and documentation**

Generalize the existing suggestion dock without changing its border geometry or
palette. Document `$` and `@` completion in `README.md`.

- [ ] **Step 4: Verify GREEN and regression suite**

Run: `cargo fmt --check`

Run: `cargo test`

Expected: both commands pass with no warnings or failures.
