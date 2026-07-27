# Vibe mode

## Goal

Replace the visible Response, Shell, and Diff controls with one `Vibe mode` control above the composer.

## Modes

- `Vibe` (default): Response `Short`, Shell `Collapse`, Diff `Collapse`.
- `Super Vibe`: Response `Short`, Shell `Hide`, Diff `Hide`.
- `Custom`: retains independently selected Response, Shell, and Diff values.

## Interaction

The composer displays only `Vibe mode: <mode>`. Clicking it cycles Vibe, Super Vibe, and Custom. Selecting either preset applies all three underlying settings together. Selecting Custom preserves the current values.

Response, Shell, and Diff are not separately exposed in the composer. Any slash command that changes one of those settings changes the active mode to Custom.

## Data and persistence

Store `VibeMode` independently from the three underlying settings so Custom remains distinguishable even when its values match a preset. Initialize new state as Vibe. Persist preset-driven Shell and Diff changes through the existing configuration actions.

## Verification

Focused state and renderer tests cover defaults, preset values, a slash-command transition to Custom, and the single visible composer badge.
