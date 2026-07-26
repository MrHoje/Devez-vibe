# Sticky prompt header

## Scope

Add Claude Code-style sticky user-prompt context to the fullscreen transcript.
Inline mode is excluded because the terminal, rather than DevezCLI, owns its
scrollback.

## Behaviour

- While scrolling upward, show the most recent user prompt above the visible
  transcript once its original prompt block has moved above the viewport.
- Use the existing `❯` user-prompt treatment in a compact, one-row header.
- Hide the header when the original prompt is visible, no earlier user prompt
  exists, or the viewport follows the live end.
- Reserve one transcript row for a shown header without moving the composer or
  changing the user's intended scroll position.

## Design

When transcript blocks are wrapped, record a parallel list of user-prompt row
anchors: each anchor contains the rendered row range and compact prompt text.
The renderer finds the last anchor ending at or before the transcript viewport
start. If the viewport starts after that prompt block, it prepends the sticky
row and reduces the ordinary transcript slice by one row. The cache is rebuilt
with the wrapped transcript on width, history, or display-mode changes.

## Verification

Unit tests cover anchor selection, no duplication while the source prompt is
visible, next-prompt replacement, and preservation of bottom-anchored composer
layout.
