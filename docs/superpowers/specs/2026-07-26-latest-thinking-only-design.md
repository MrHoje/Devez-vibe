# Compact Thinking and Shell Display Design

## Goal

Prevent repeated reasoning updates from filling the transcript by keeping only
the latest `Thinking…` block in each uninterrupted reasoning run. Keep Shell
headings compact by using the same count-and-status summary for both single and
multi-command executions.

## Behavior

- Consecutive `Thinking…` blocks in the same turn collapse to the last block.
- A Shell block, file change, assistant message, plan, or any other visible
  block ends the reasoning run.
- A later `Thinking…` block after such a boundary starts a new run and remains
  visible independently.
- Live output and resumed session history use the same rule.
- An empty latest reasoning block still renders the existing `Thinking…` label.
- `Plan` blocks remain unchanged even though they also use `BlockKind::Reasoning`
  in some protocol paths; only the exact `Thinking…` title participates.

## Shell Summary

- One successful or status-unknown command renders
  `▸ Shell · 1 command · completed · <duration>`.
- Multiple successful or status-unknown commands render
  `▸ Shell · N commands · completed · <duration>`.
- Any failures render
  `▸ Shell · N commands · M failed · <duration>` with warning styling.
- Successful and unknown results use the same `completed` label; missing
  duration is omitted.
- The collapsed heading never exposes the executable path or command text.
- The collapsed heading always occupies one physical terminal row. When it is
  wider than the available cells, the renderer truncates it with an ellipsis
  instead of wrapping.
- Expanding the heading shows the actual command and its output, including for
  a one-command group; expanded details may use multiple rows.

## Architecture

Add one small normalization helper in the state layer. It accepts blocks in
display order and replaces the immediately preceding block only when both blocks
are exact `Thinking…` reasoning blocks. All other blocks are appended normally
and therefore act as boundaries.

Use the helper at both transcript entry points:

- when completed live items are committed;
- after resumed turn items and rollout events have been timestamp-merged.

The renderer remains responsible only for painting one reasoning block as its
existing dim italic paragraph. It does not infer history relationships.

Shell result construction always creates a Shell group, including when it has
one child. The existing group renderer then supplies the common summary heading
and reveals child command details only while expanded.

## Edge Cases

- Identical and different consecutive summaries both keep only the latest.
- A blank latest summary replaces an earlier non-empty summary.
- Reasoning updates from different turns never replace each other.
- Active streaming deltas remain one active block and need no extra merging.
- Non-`Thinking…` reasoning blocks are preserved.
- A single live or resumed command uses singular `command`.
- Single-command failures use `1 command · 1 failed`.

## Tests

- Two consecutive completed Thinking blocks produce one block containing the
  latest summary.
- A Shell block between Thinking blocks preserves both reasoning blocks.
- Resumed rollout reasoning blocks follow the same rule.
- Plan and other reasoning-titled blocks are not collapsed.
- Empty latest Thinking renders the existing label.
- Single live and resumed Shell results hide their command path when collapsed.
- Long collapsed Shell summaries remain one physical row.
- Expanding a single Shell summary reveals the original command and output.
- Singular/plural, success, failure, unknown status, and missing duration
  summaries render correctly.
