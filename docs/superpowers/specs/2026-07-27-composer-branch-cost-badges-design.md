# Composer branch and cost badges

## Goal

Place the Git branch and estimated cost on the composer top rule immediately before the response-length control.

## Layout

At sufficient width, the rule displays this fixed order:

`main / [0.95$] / Response: Short / Shell: Hide / Diff: Hide / Fast: Off`

The branch and cost are display-only. Response, Shell, Diff, and Fast retain their existing click targets.

## Responsive behavior

When width is constrained, retain the leading branch, cost, and Response labels. Hide optional trailing controls in this order: Fast, Diff, then Shell. Do not truncate individual labels.

## Data flow

Pass the current branch into the composer display model, alongside the existing estimated cost, and remove the branch from the status row so it is not shown twice.

## Verification

Renderer tests cover full-width ordering, narrow-width degradation, and that only the four existing controls remain clickable.
