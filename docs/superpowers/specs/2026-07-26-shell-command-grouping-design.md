# Shell Command Grouping Design

## Goal

Reduce repeated `Shell` headings by presenting commands from the same execution
request as one collapsible group.

## Interaction

- A request containing one shell command keeps the existing single-command row.
- A request containing two or more shell commands renders one collapsed row:
  `▸ Shell · N commands · all passed · <duration>`.
- If any command fails, the summary becomes
  `▸ Shell · N commands · M failed · <duration>` and uses warning styling.
- Clicking the group heading in fullscreen mode expands or collapses the group.
- Expanded groups show each existing command heading and its output using the
  current per-command expansion presentation.
- Commands separated by assistant text, reasoning, file changes, or another
  execution request remain in separate groups.
- Inline mode keeps groups collapsed, matching existing shell-output behavior.

## Group Identity and Data Flow

Grouping follows the protocol execution-request boundary, not visual adjacency.
All `shell_command` calls extracted from one `exec` call share one group. The
state layer preserves that relationship when converting active, completed, and
resumed rollout events into renderable blocks.

A grouped block carries:

- the ordered commands;
- each command's output, exit code, and duration;
- aggregate duration when available;
- aggregate success or failure count.

Single-command blocks continue through the existing path so their appearance
and click behavior do not change.

## Rendering

The renderer recognizes grouped shell blocks separately from ordinary shell
blocks. A collapsed group emits only its summary heading. An expanded group
emits the summary heading followed by every child command heading; child output
is shown beneath its command using the existing muted shell-output styling.

The group heading owns the click target. Child rows do not introduce nested
collapse state, keeping the interaction predictable and avoiding two levels of
disclosure.

Long group headings wrap using the existing heading renderer. Every wrapped
heading row remains clickable.

## Edge Cases

- Missing exit code: do not count that command as failed; use `completed` rather
  than `all passed` when not every result is known.
- Missing duration: omit aggregate duration.
- Mixed success and failure: show the number failed and warning styling.
- Empty command output: show the command heading without body rows when expanded.
- Resumed sessions: reconstruct groups from each rollout `exec` call so grouping
  matches the original live session.
- Active parallel commands: retain the existing `Running N shell commands`
  status until the request completes, then replace it with the grouped result.

## Tests

- One command retains the existing single-command row.
- Two or more commands from one execution request become one collapsed group.
- Commands from separate execution requests are not merged.
- Intervening reasoning or assistant text preserves group boundaries.
- Expanding a group reveals commands in execution order and their outputs.
- All-success, mixed-failure, unknown-status, and missing-duration summaries are
  rendered correctly.
- Warning styling is applied when at least one command fails.
- Resumed rollout events reconstruct the same grouping as live events.
- Fullscreen heading clicks toggle a group; inline mode remains collapsed.
