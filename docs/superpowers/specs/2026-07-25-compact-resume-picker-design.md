# Compact Resume Picker Design

## Goal

Render the session picker as a compact dock instead of a tall panel, matching
the command suggestion UI while keeping session search and navigation.

## Behavior

- Apply the same picker to runtime `/resume` and startup `dvz --resume`.
- Show at most 10 visible sessions and scroll the window with the selection.
- Keep every session on one physical row.
- Format rows as relative time followed by the session name or first prompt.
- In all-projects mode, append the session folder on the same row.
- Truncate overflowing content on the right with an ellipsis.
- Keep search, Up/Down, Enter, Ctrl+A, PageUp/PageDown, and Esc behavior.
- Preserve the conversation or welcome content above the dock.

## Implementation

`SessionPicker` remains responsible for filtering, selection, and row content.
It uses a resume-specific 10-row window and produces single-line rows.

Add a compact overlay style in `renderer.rs`. It shares the bordered visual
language and right-side truncation behavior of command suggestions, but retains
overlay input support for the session search field. Existing panel and picker
overlays are unchanged.

## Verification

- State tests verify the 10-row window, time-first row order, and one-line
  all-projects formatting.
- Renderer tests verify compact rows never wrap and that the dock preserves
  content above it.
- Run the focused state and renderer tests, then the full test suite.
