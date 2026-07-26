# Shell Command Grouping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Collapse two or more Shell commands from one execution batch into one clickable summary row.

**Architecture:** Preserve an execution-group identity in rollout parsing and track overlapping live command executions as one batch in application state. Represent a completed group as one `Block` containing ordered child Shell blocks; the renderer reuses its existing expansion state and click hit map to show or hide those children.

**Tech Stack:** Rust, serde_json, crossterm, existing unit-test modules in `src/rollout.rs`, `src/state.rs`, and `src/renderer.rs`.

## Global Constraints

- One Shell command keeps the existing presentation.
- Two or more commands render as `▸ Shell · N commands · all passed · <duration>`.
- A mixed result renders `▸ Shell · N commands · M failed · <duration>` with warning styling.
- Unknown statuses use `completed`; missing aggregate duration is omitted.
- Reasoning, assistant messages, file changes, and distinct execution batches remain boundaries.
- Fullscreen headings toggle expansion; inline mode remains collapsed.
- Expanded groups show ordered child command headings and complete non-empty output.
- Add no dependency.

---

### Task 1: Add a grouped Shell block to the renderer model

**Files:**
- Modify: `src/renderer.rs` (`Block`, Shell block rendering, renderer tests)

**Interfaces:**
- Consumes: existing `Block::new`, `Block::id`, `expanded_tools`, and `PaintLine::tool_heading`.
- Produces: `Block::shell_group(kind: BlockKind, title: String, children: Vec<Block>) -> Block` and `Block::children(&self) -> &[Block]`.

- [ ] **Step 1: Write failing renderer tests**

Add tests beside the existing collapsible Shell tests:

```rust
#[test]
fn collapsed_shell_group_is_one_clickable_row() {
    let group = Block::shell_group(
        BlockKind::Tool,
        "Shell · 2 commands · all passed · 1.2s".to_owned(),
        vec![
            Block::new(BlockKind::Tool, "Shell · first · exit 0", "one"),
            Block::new(BlockKind::Tool, "Shell · second · exit 0", "two"),
        ],
    );

    let lines = block_lines(&group, 80);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "Shell · 2 commands · all passed · 1.2s");
    assert_eq!(lines[0].tool_heading, Some(group.id()));
}

#[test]
fn expanded_shell_group_shows_ordered_children_without_nested_click_targets() {
    let group = Block::shell_group(
        BlockKind::Tool,
        "Shell · 2 commands · all passed".to_owned(),
        vec![
            Block::new(BlockKind::Tool, "Shell · first · exit 0", "one"),
            Block::new(BlockKind::Tool, "Shell · second · exit 0", "two"),
        ],
    );

    let lines = block_lines_with_expansion(&group, 80, true);
    let text = lines.iter().map(|line| line.text.as_str()).collect::<Vec<_>>();
    assert_eq!(
        text,
        [
            "Shell · 2 commands · all passed",
            "Shell · first · exit 0",
            "one",
            "Shell · second · exit 0",
            "two",
        ]
    );
    assert_eq!(lines[0].tool_heading, Some(group.id()));
    assert!(lines[1..].iter().all(|line| line.tool_heading.is_none()));
}
```

- [ ] **Step 2: Run the tests and verify RED**

Run:

```powershell
cargo test shell_group
```

Expected: compilation fails because `Block::shell_group` and `Block::children` do not exist.

- [ ] **Step 3: Add child storage and the group constructor**

Extend `Block` without changing ordinary call sites:

```rust
pub struct Block {
    id: u64,
    pub kind: BlockKind,
    pub title: String,
    pub body: String,
    children: Vec<Block>,
}

pub fn new(...) -> Self {
    Self {
        id: NEXT_BLOCK_ID.fetch_add(1, Ordering::Relaxed),
        kind,
        title: title.into(),
        body: body.into(),
        children: Vec::new(),
    }
}

pub fn shell_group(kind: BlockKind, title: String, children: Vec<Block>) -> Self {
    let mut block = Self::new(kind, title, "");
    block.children = children;
    block
}

pub fn children(&self) -> &[Block] {
    &self.children
}
```

- [ ] **Step 4: Render children only when the group is expanded**

Update `bash_lines` so the parent heading remains the sole click target. For a
group, append each child's heading with an indented bullet and then every
non-empty output row with the current muted styling. Do not call `bash_lines`
recursively because that would install nested `tool_heading` IDs.

- [ ] **Step 5: Run renderer tests**

Run:

```powershell
cargo test renderer::tests
```

Expected: all renderer tests pass, including existing single-command expansion and click tests.

- [ ] **Step 6: Commit**

```powershell
git add src/renderer.rs
git commit -m "feat(renderer): render collapsible shell groups"
```

---

### Task 2: Preserve rollout execution-request groups

**Files:**
- Modify: `src/rollout.rs` (`RolloutKind::Exec`, parser, parser tests)
- Modify: `src/state.rs` (`event_block` replacement, resumed-history grouping, state tests)

**Interfaces:**
- Consumes: `shell_commands`, `command_results`, `format_duration`, `collapse_output`, and `Block::shell_group`.
- Produces: `RolloutKind::Exec { group_id: String, command, output, exit_code, duration_ms }` and `shell_group_block(children: Vec<Block>) -> Block`.

- [ ] **Step 1: Write failing rollout identity test**

Extend the multi-command parser test to collect group IDs:

```rust
let groups = rollout.events.iter().filter_map(|event| match &event.kind {
    RolloutKind::Exec { group_id, .. } => Some(group_id.as_str()),
    _ => None,
}).collect::<Vec<_>>();
assert_eq!(groups, ["call_pair", "call_pair"]);
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```powershell
cargo test a_promise_all_script_produces_one_exec_event_per_call
```

Expected: compilation fails because `RolloutKind::Exec` has no `group_id`.

- [ ] **Step 3: Preserve `call_id` on every parsed child event**

Add `group_id: String` to `RolloutKind::Exec`. When parsing one
`custom_tool_call`, clone its `call_id` into every event created for that
call. Update existing match expressions and test helpers to ignore or expose the
new field explicitly.

- [ ] **Step 4: Write failing resumed-group state tests**

Add a history test using one `custom_tool_call(exec)` containing two
`shell_command` calls and one paired output. Assert:

```rust
let shell = state.committed.iter()
    .find(|block| block.title.starts_with("Shell · 2 commands"))
    .expect("grouped shell block");
assert_eq!(shell.title, "Shell · 2 commands · 1 failed · 4.1s");
assert!(matches!(shell.kind, BlockKind::Warning));
assert_eq!(shell.children().len(), 2);
assert_eq!(shell.children()[0].title, "Shell · rg TODO · exit 0 · 4.1s");
assert_eq!(shell.children()[1].title, "Shell · git status --short · exit 1 · 4.1s");
```

Add a second test with two separate `call_id` values at adjacent timestamps and
assert that they remain two ordinary single-command blocks.

- [ ] **Step 5: Run the state tests and verify RED**

Run:

```powershell
cargo test resumed_multi_command_exec
cargo test separate_exec_calls
```

Expected: the first test finds separate Shell rows instead of one group.

- [ ] **Step 6: Build grouped rollout blocks before timestamp sorting**

Replace the one-event-at-a-time conversion in `merged_turn_blocks` with a helper
that groups `RolloutKind::Exec` events by `group_id`, preserving first-event
order and command order. Convert a one-child group through the existing
single-command path. Convert two or more children through
`Block::shell_group`.

Use this summary rule:

```rust
let status = if failed > 0 {
    format!("{failed} failed")
} else if known == count {
    "all passed".to_owned()
} else {
    "completed".to_owned()
};
```

Use the shared wrapper wall time once as the group duration. Keep the existing
duration on child titles because it is already part of the stored result.

- [ ] **Step 7: Run rollout and state tests**

Run:

```powershell
cargo test rollout::tests
cargo test state::tests
```

Expected: all tests pass.

- [ ] **Step 8: Commit**

```powershell
git add src/rollout.rs src/state.rs
git commit -m "feat(history): group shell runs by exec request"
```

---

### Task 3: Group live overlapping Shell executions

**Files:**
- Modify: `src/state.rs` (`AppState` fields, `start_item`, `complete_item`, turn cleanup, state tests)

**Interfaces:**
- Consumes: existing active item IDs, `completed_item_block`, and `shell_group_block`.
- Produces: renderer-ready grouped blocks for live Shell commands without changing app-server payloads.

- [ ] **Step 1: Write failing live state tests**

Start two `commandExecution` items before completing either. Complete them with
exit codes `0` and `1`, then assert that `committed` contains one group with two
children, warning styling, and `1 failed`.

Add a single-command test that starts and completes one item and asserts the
existing title and block ID remain unchanged.

Add a boundary test that completes one command before the next command starts
and asserts two ordinary blocks. This prevents unrelated sequential executions
from being merged.

- [ ] **Step 2: Run the tests and verify RED**

Run:

```powershell
cargo test live_overlapping_shells
cargo test live_single_shell
cargo test sequential_shells
```

Expected: the overlapping commands commit as two blocks.

- [ ] **Step 3: Track live batch membership**

Add a private `ShellBatch` containing ordered member IDs and completed child
blocks. When a Shell item starts:

- create a batch keyed by its first member when no Shell is active;
- join the currently active Shell batch when another Shell is active;
- record each item ID's batch key.

When a member completes, store its completed block in member order. When every
member has completed:

- commit the child directly when the batch has one member;
- commit `shell_group_block(children)` when it has two or more;
- clear batch and membership state.

Flush incomplete members as ordinary blocks during existing orphan cleanup so a
cancelled turn cannot hide completed command results.

- [ ] **Step 4: Preserve stable expansion identity**

For a one-command batch, retain the active block's ID exactly as today. For a
group, use the first child block's ID as the group ID via `adopt_id`; this keeps
the heading stable as the live batch becomes a completed transcript row.

- [ ] **Step 5: Run state tests**

Run:

```powershell
cargo test state::tests
```

Expected: all state tests pass, including the existing
`active_shell_commands_are_grouped_into_one_status_row` test.

- [ ] **Step 6: Commit**

```powershell
git add src/state.rs
git commit -m "feat(state): group overlapping live shell executions"
```

---

### Task 4: Full verification

**Files:**
- Verify: `src/renderer.rs`
- Verify: `src/rollout.rs`
- Verify: `src/state.rs`

**Interfaces:**
- Consumes: completed Tasks 1–3.
- Produces: formatted, warning-free, fully tested feature.

- [ ] **Step 1: Format and inspect the diff**

Run:

```powershell
cargo fmt --check
git diff --check
```

Expected: both commands exit successfully. If formatting fails, run `cargo fmt`
and rerun both checks.

- [ ] **Step 2: Run the complete test suite**

Run:

```powershell
cargo test
```

Expected: all tests pass with no new warnings.

- [ ] **Step 3: Check the requested visual shape**

Run DevezCLI in fullscreen mode and trigger two parallel command batches, one
with three commands and one with two commands separated by assistant reasoning.
Expected transcript:

```text
▸ Shell · 3 commands · all passed · …

∴ <reasoning summary>

▸ Shell · 2 commands · all passed · …
```

Click each group and confirm only that group's ordered commands and output
expand. Confirm a failed child changes the summary to warning styling and
`N failed`.

- [ ] **Step 4: Commit any verification-only adjustments**

```powershell
git add src/renderer.rs src/rollout.rs src/state.rs
git commit -m "test: cover grouped shell command results"
```
