# Latest Thinking Only Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep only the latest block in each uninterrupted run of `Thinking…` updates.

**Architecture:** Add a state-layer block insertion helper that replaces the immediately preceding block only when both are exact `Thinking…` reasoning blocks. Use it for live completed/orphaned items and normalize each resumed turn after timestamp sorting.

**Tech Stack:** Rust, serde_json, existing unit tests in `src/state.rs`.

## Global Constraints

- Only `BlockKind::Reasoning` blocks titled exactly `Thinking…` participate.
- Shell, file change, assistant, plan, and all other blocks are boundaries.
- Different turns never replace each other's Thinking block.
- The renderer remains unchanged.
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
