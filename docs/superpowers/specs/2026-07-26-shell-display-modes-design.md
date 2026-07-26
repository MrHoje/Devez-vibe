# Shell display modes

## Goal

Give the transcript one global Shell visibility control in the composer rule,
between estimated cost and permission mode. It lets the user keep Shell output
out of the way without losing the chronological completion records.

## Modes

- **Hide**: render no completed Shell blocks.
- **Collapse** (default): render each completed Shell block as its one-line
  summary.
- **Expand**: render the summary and at most five painted output rows across
  the entire Shell group. A grouped execution shares the five-row budget among
  all child commands, in command order.

Clicking the `Shell: <mode>` composer badge cycles Hide → Collapse → Expand →
Hide. On narrow terminals, the estimated-cost badge is removed before this
control or permission mode.

## Completion placement

An active request keeps its Shell completion record at the place its Shell
execution began. Once every Shell command in that request has completed, its
summary changes there to `completed` (or a failure summary). It is never moved
to the newest end of the transcript merely because it completed later.

## Rendering and interaction

The mode applies to completed Shell groups globally. A click on a visible Shell
summary still expands that individual group to all of its output, independent
of the global preview mode. Hide disables those rows and their click targets.

## Verification

Renderer tests cover the mode badge, the narrow-width omission order, hiding,
one-line collapse, and the five-painted-row global preview cap. State tests
cover completion remaining in the Shell's original transcript position.
