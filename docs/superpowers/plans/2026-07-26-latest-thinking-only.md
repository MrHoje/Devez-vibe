# Compact Thinking and Shell Display Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep only the latest uninterrupted `Thinking…` update and hide completed Shell command paths behind count summaries.

**Architecture:** Add a state-layer block insertion helper that replaces the immediately preceding block only when both are exact `Thinking…` reasoning blocks. Normalize completed Shell results through the existing group block for every result count, including one command, so the renderer shows details only after expansion.

**Tech Stack:** Rust, serde_json, existing unit tests in `src/state.rs`.

## Global Constraints

- Only `BlockKind::Reasoning` blocks titled exactly `Thinking…` participate.
- Shell, file change, assistant, plan, and all other blocks are boundaries.
- Different turns never replace each other's Thinking block.
- The renderer remains unchanged.
- A completed single Shell result renders `Shell · 1 command · <status>`.
- Collapsed Shell headings never contain an executable path or command text.
- Successful and status-unknown results both use `completed`.
- Collapsed Shell headings always occupy one physical row and ellipsize.
- Expanded Shell headings reveal the original command and output.
- Add no dependency.

---

### Task 1: Normalize consecutive Thinking blocks

**Files:**
- Modify: `src/state.rs`
- Test: `src/state.rs` test module

**Interfaces:**
- Consumes: `Block`, `BlockKind`, `complete_item`, `flush_orphaned_active`, and `merged_turn_blocks`.
- Produces: `push_latest_thinking(blocks: &mut Vec<Block>, block: Block)`.

- [ ] **Step 1: Write failing live-state tests**

Add:

```rust
#[test]
fn consecutive_live_thinking_keeps_only_the_latest() {
    let mut state = test_state();
    for (id, summary) in [("r1", "first"), ("r2", "latest")] {
        state.complete_item(&json!({
            "id": id,
            "type": "reasoning",
            "summary": [summary]
        }));
    }

    assert_eq!(state.committed.len(), 1);
    assert_eq!(state.committed[0].title, "Thinking…");
    assert_eq!(state.committed[0].body, "latest");
}

#[test]
fn shell_between_thinking_blocks_preserves_both() {
    let mut state = test_state();
    state.complete_item(&json!({"id":"r1","type":"reasoning","summary":["first"]}));
    state.complete_item(&json!({
        "id":"cmd","type":"commandExecution","command":"pwd",
        "status":"completed","exitCode":0
    }));
    state.complete_item(&json!({"id":"r2","type":"reasoning","summary":["second"]}));

    let thinking = state.committed.iter()
        .filter(|block| block.title == "Thinking…")
        .count();
    assert_eq!(thinking, 2);
}
```

- [ ] **Step 2: Run tests and verify RED**

Run:

```powershell
cargo test consecutive_live_thinking
cargo test shell_between_thinking
```

Expected: the first test reports two committed blocks; the boundary test passes.

- [ ] **Step 3: Add the minimal insertion helper**

Add:

```rust
fn is_thinking(block: &Block) -> bool {
    matches!(block.kind, BlockKind::Reasoning) && block.title == "Thinking…"
}

fn push_latest_thinking(blocks: &mut Vec<Block>, block: Block) {
    if is_thinking(&block) && blocks.last().is_some_and(is_thinking) {
        blocks.pop();
    }
    blocks.push(block);
}
```

Use `push_latest_thinking` instead of direct pushes in `complete_item` and for
non-Shell blocks emitted by `flush_orphaned_active`.

- [ ] **Step 4: Run live tests and verify GREEN**

Run:

```powershell
cargo test consecutive_live_thinking
cargo test shell_between_thinking
```

Expected: both tests pass.

- [ ] **Step 5: Write failing resumed-turn tests**

Add a pure normalization test:

```rust
#[test]
fn resumed_turn_keeps_latest_consecutive_thinking_per_run() {
    let blocks = latest_thinking_only(vec![
        Block::new(BlockKind::Reasoning, "Thinking…", "first"),
        Block::new(BlockKind::Reasoning, "Thinking…", "latest"),
        Block::new(BlockKind::Tool, "Shell · pwd", ""),
        Block::new(BlockKind::Reasoning, "Thinking…", "after shell"),
    ]);

    let bodies = blocks.iter()
        .filter(|block| block.title == "Thinking…")
        .map(|block| block.body.as_str())
        .collect::<Vec<_>>();
    assert_eq!(bodies, ["latest", "after shell"]);
}
```

- [ ] **Step 6: Run resumed test and verify RED**

Run:

```powershell
cargo test resumed_turn_keeps_latest_consecutive_thinking_per_run
```

Expected: compilation fails because `latest_thinking_only` does not exist.

- [ ] **Step 7: Normalize each merged resumed turn**

Add:

```rust
fn latest_thinking_only(blocks: Vec<Block>) -> Vec<Block> {
    blocks.into_iter().fold(Vec::new(), |mut normalized, block| {
        push_latest_thinking(&mut normalized, block);
        normalized
    })
}
```

In `merged_turn_blocks`, collect the sorted blocks and pass that vector through
`latest_thinking_only` before returning it. Because this function is called once
per turn, reasoning never collapses across turn boundaries.

- [ ] **Step 8: Run focused and full tests**

Run:

```powershell
cargo test thinking
cargo test --quiet
git diff --check
```

Expected: all tests pass and the diff check exits successfully.

- [ ] **Step 9: Commit only the feature files if the mixed working tree permits**

Do not stage unrelated concurrent edits. If `src/state.rs` contains inseparable
user changes, leave the implementation uncommitted and report that explicitly.

---

### Task 2: Summarize completed single Shell commands

**Files:**
- Modify: `src/state.rs`
- Test: `src/state.rs` test module

**Interfaces:**
- Consumes: existing `ShellResult`, `shell_results_block`, and
  `Block::shell_group`.
- Produces: the same grouped Shell block shape for one or more results.

- [ ] **Step 1: Write a failing live single-command test**

Add:

```rust
#[test]
fn live_single_shell_hides_its_command_in_the_summary() {
    let mut state = test_state();
    state.start_item(&json!({
        "id":"cmd-1","type":"commandExecution",
        "command":"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe -Command Get-Content"
    }));
    state.complete_item(&json!({
        "id":"cmd-1","type":"commandExecution",
        "command":"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe -Command Get-Content",
        "status":"completed","exitCode":0,"durationMs":670,
        "aggregatedOutput":"contents"
    }));

    let shell = state.committed.last().expect("completed shell");
    assert_eq!(shell.title, "Shell · 1 command · all passed · 670ms");
    assert_eq!(shell.children().len(), 1);
    assert!(shell.children()[0].title.contains("powershell.exe"));
    assert_eq!(shell.children()[0].body, "contents");
}
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```powershell
cargo test live_single_shell_hides_its_command_in_the_summary
```

Expected: the title still contains the executable path and the block has no
children.

- [ ] **Step 3: Always construct a Shell group**

Remove the one-result early return from `shell_results_block`. Use singular and
plural nouns in the common title:

```rust
let count = results.len();
let noun = if count == 1 { "command" } else { "commands" };
// Keep the existing status and duration calculations.
Block::shell_group(
    kind,
    format!("Shell · {count} {noun} · {status}{duration}"),
    children,
)
```

The single child retains the existing detailed command title and output, which
the renderer reveals only when the parent is expanded.

- [ ] **Step 4: Update resumed single-command expectations**

Change resumed Shell assertions from detailed collapsed titles such as
`Shell · cargo test · exit 0 · 1.6s` to
`Shell · 1 command · all passed · 1.6s`. Assert that the original detailed
title is present in `children()[0]`.

For a failed result expect
`Shell · 1 command · 1 failed · <duration>` and retain warning styling.

- [ ] **Step 5: Run focused tests and verify GREEN**

Run:

```powershell
cargo test live_single_shell
cargo test resumed_shell
cargo test failed_shell
```

Expected: all focused tests pass.

- [ ] **Step 6: Run full verification**

Run:

```powershell
cargo test --quiet
git diff --check
```

Expected: all tests pass and the diff check exits successfully.

---

### Task 3: Unify completion status and keep collapsed headings on one row

**Files:**
- Modify: `src/state.rs`
- Modify: `src/renderer.rs`
- Test: existing test modules in both files

**Interfaces:**
- Consumes: `shell_results_block`, `bash_lines`, `compact_right`, and
  `PaintLine::tool_heading`.
- Produces: one `completed` label for successful/unknown results and a
  single-row collapsed Shell heading.

- [ ] **Step 1: Write failing status tests**

Update successful live and resumed Shell expectations from `all passed` to
`completed`. Keep failed expectations as `N failed`. Add an unknown-exit test
and assert it produces the same `completed` title as a successful result.

- [ ] **Step 2: Run status tests and verify RED**

Run:

```powershell
cargo test live_single_shell
cargo test resumed_shell
```

Expected: successful titles still contain `all passed`.

- [ ] **Step 3: Unify non-failure status**

In `shell_results_block`, replace the known-success/unknown branch with:

```rust
let status = if failed > 0 {
    format!("{failed} failed")
} else {
    "completed".to_owned()
};
```

Remove the now-unused `known` calculation.

- [ ] **Step 4: Write a failing one-row renderer test**

Add:

```rust
#[test]
fn collapsed_shell_heading_ellipsizes_instead_of_wrapping() {
    let block = Block::shell_group(
        BlockKind::Tool,
        "Shell · 123 commands · completed · 123.4s",
        vec![Block::new(BlockKind::Tool, "Shell · detail", "")],
    );
    let lines = block_lines(&block, 20);

    assert_eq!(lines.len(), 1);
    assert!(painted_line_width(&lines[0]) <= 20);
    assert!(lines[0].text.ends_with('…'));
    assert_eq!(lines[0].tool_heading, Some(block.id()));
}
```

- [ ] **Step 5: Run renderer test and verify RED**

Run:

```powershell
cargo test collapsed_shell_heading_ellipsizes
```

Expected: the heading wraps to multiple rows.

- [ ] **Step 6: Render the collapsed heading as one compact row**

In `bash_lines`, when `expanded` is false, reserve the marker width and apply
`compact_right` to the title. Return one `PaintLine` carrying the block ID as
its click target. Keep the existing expanded child rendering unchanged.

- [ ] **Step 7: Run focused and full verification**

Run:

```powershell
cargo test shell
cargo test --quiet
git diff --check
```

Expected: all tests pass and every collapsed Shell heading remains one row.
