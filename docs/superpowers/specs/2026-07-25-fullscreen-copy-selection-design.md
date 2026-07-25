# Fullscreen-owned copy selection

## Goal

Show a copy notice only when the current `dvz` session copied text. Copying in
another Codex session or any other process must never change this session's UI.

## Root cause

`ClipboardWatcher` polls the operating system's global clipboard. A clipboard
revision identifies that some process wrote to the clipboard, but it carries no
terminal, pane, process, or Codex thread identity. Consequently every running
`dvz` instance treats every non-empty text write as its own copy.

## Chosen approach

Match Claude Code's fullscreen behavior: the fullscreen renderer owns mouse
selection and performs the clipboard write itself. The application displays a
notice only after that local write succeeds.

Foreground-window filtering is rejected because multiple Windows Terminal tabs
and panes share one top-level window. Clipboard-content correlation is rejected
because identical text may be copied by another process and native terminal
selection provides no reliable source token.

## Interaction

- Fullscreen mode enables terminal mouse reporting.
- Left-button down records a potential selection anchor.
- Left-button drag updates the focused cell and paints the selected range.
- Left-button up after movement copies the selected text without clearing the
  highlight and shows `Copied N chars to clipboard`.
- A down/up pair on the same cell is a click, not a copy. It remains available
  to renderer click targets such as collapsible tool headings.
- Starting another selection, pressing a normal key, scrolling, resizing, or
  replacing the visible screen clears a stale selection.
- Shift+drag remains the terminal-native selection escape hatch. Modifier-tagged
  mouse events are not treated as application selection.
- Inline mode never enables mouse reporting. Native selection continues to
  belong to the terminal and produces no `dvz` notice.
- `/copy` remains unchanged: it writes the last response itself and shows a
  notice only after success.
- Empty and whitespace-only selections do not write or show a notice.

## Architecture

### Terminal lifecycle

`TerminalSession` enables mouse capture only after entering fullscreen and
disables it before leaving fullscreen. Alternate-scroll mode 1007 is still
saved, disabled, and restored in the existing order. Inline setup and teardown
remain unchanged.

### Selection model

`Renderer` owns a small `Selection` state:

- zero-based screen-cell `anchor`,
- zero-based screen-cell `focus`,
- whether the left button is currently dragging,
- the last selection painted to the terminal.

The current fullscreen `previous_lines` buffer is the selection source. Mouse
coordinates are clamped to its row and display-column bounds. Normalizing the
two endpoints supports forward, backward, and multi-line drags.

Renderer methods expose bounded outcomes to the event loop:

- begin or update selection and request repaint,
- finish selection and return either copied text, a click row, or nothing,
- clear selection and report whether repaint is needed.

The application event loop owns clipboard access. This keeps OS I/O and user
notices out of renderer logic and makes extraction independently testable.

### Text reconstruction

Each selected row is flattened from its painted prefix, main text, and visible
tail spans. `CopyJoin` is metadata and is never emitted as text.

Display-column slicing uses Unicode cell width rather than byte or scalar
indices. Selecting either cell occupied by a wide character includes that
character once. Zero-width combining characters stay attached to their base
character.

For multi-line selection:

- endpoint cells are inclusive,
- unselected leading and trailing cells are omitted,
- a newline is inserted between independent painted rows,
- rows marked `CopyJoin` join directly to their wrapped continuation,
- decorative conversation markers are omitted when the selected range includes
  the complete marker.

Trailing terminal padding is not present in `PaintLine` and therefore is never
copied.

### Painting

Selection highlighting is applied while printing a fullscreen row, without
mutating the underlying `PaintLine`. Rows whose selection range changed are
repainted even when their text is identical. The current theme's selection
background is used when available; otherwise reverse video supplies a readable
terminal-native fallback.

## Mouse routing

The fullscreen event loop routes mouse input in this order:

1. modifier/native-selection escape,
2. wheel scrolling,
3. left-button selection down/drag/up,
4. same-cell click dispatch,
5. ignore unsupported mouse events.

This order prevents wheel input from reaching prompt history and lets a future
or concurrent clickable-tool implementation share the same capture mode.

## Error handling

- Clipboard creation or write failure displays the existing `복사 실패` error
  notice and does not display success.
- Mouse events received without a rendered fullscreen screen are ignored.
- Coordinates outside the current screen are clamped safely.
- Screen changes during a drag cancel the selection rather than copying text
  from mismatched rows.

## Testing

Pure renderer tests cover:

- terminal mouse capture is fullscreen-only and teardown is symmetric,
- forward and backward single-line selection,
- multi-line selection and inclusive endpoints,
- Unicode wide and combining characters,
- visual-wrap joining,
- decorative marker removal,
- empty/same-cell clicks do not copy,
- selection ranges repaint when text is unchanged,
- resize, scroll, and screen replacement clear stale selection,
- wheel routing never reaches editor history,
- inline mode does not route application selection,
- another process's clipboard revision has no event path after
  `ClipboardWatcher` is removed.

Focused integration tests verify that a local successful selection write sets
the notice and a failed write sets only the error notice. The full Rust test
suite and formatter run after integration.

## Scope

This change removes global clipboard polling and adds fullscreen-owned text
selection. It does not add clipboard history, a new setting, word/line
double-click selection, or application-owned selection in inline mode.
