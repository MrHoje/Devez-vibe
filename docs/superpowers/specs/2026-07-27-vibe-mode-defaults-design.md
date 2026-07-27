# Vibe mode defaults

## Goal

Persist every Vibe mode change as the default for future sessions.

## Persistence

Vibe and Super Vibe persist their mode plus Response, Shell, and Diff values. Custom controls persist Custom plus all three current values. `/vibemode` persists the same data when Enter confirms selection.

## Startup

New sessions read the persisted Vibe mode and its three component values instead of always starting at Vibe.

## Verification

Focused parsing and state tests cover persisted Vibe, Super Vibe, and Custom startup values.
