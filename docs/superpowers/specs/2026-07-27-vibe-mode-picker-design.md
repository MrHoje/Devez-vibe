# Vibe mode picker

## Goal

Provide `/vibemode` as one panel for editing the Response, Shell, and Diff values that comprise Custom mode.

## Panel

The panel has three rows:

- Response: Short, Normal, Detailed
- Shell: Hide, Collapse, Expand
- Diff: Hide, Collapse, Expand

Up and Down select a row. Left and Right select a value in the active row. Values preview immediately while the panel stays open.

## Completion

Enter accepts the selections and closes the panel. Escape restores the values present when the panel opened and closes it. Any changed value sets Vibe mode to Custom.

## Verification

Focused state tests cover opening the command, row/value navigation, Enter acceptance, and Escape restoration.
