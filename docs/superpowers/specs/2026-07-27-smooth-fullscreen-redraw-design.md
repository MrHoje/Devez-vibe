# Smooth Fullscreen Redraw Design

## Goal

Make fullscreen resume and transcript scrolling appear as a single visual update on terminals that support synchronized updates, while reducing fallback output on all terminals.

## Scope

- Keep the existing alternate-screen viewport, internal scroll state, sticky prompt, mouse handling, and final-column autowrap guard.
- Replace per-cell cursor moves in `emit_frame_diff` with contiguous runs of changed, non-continuation cells that share one style.
- Bracket every fullscreen frame paint with the synchronized-update ANSI sequences (`CSI ? 2026 h` / `CSI ? 2026 l`). Terminals that do not implement the sequences ignore them and receive the same valid row-run output.
- Always end the bracket even when frame emission returns an error after it starts.

## Design

`emit_frame_diff` will scan each row left-to-right. It will start a run only at a changed, printable cell and extend it while each next cell is changed, printable, adjacent, and has the same `CellStyle`. One cursor move and style change then prints the complete run. A changed final visual cell remains a separate `Clear(UntilNewLine)` operation so the terminal is never written into its autowrap column.

`paint_screen` will emit synchronized-update start immediately before the frame diff and synchronized-update end immediately after it. The end escape is queued regardless of whether the diff succeeds, preventing a supported terminal from retaining a frozen frame after an I/O error. Cursor placement and cursor visibility remain outside the synchronized bracket.

## Error Handling

The start and end control sequences are ordinary output. Any I/O failure is returned to the caller. The end sequence is attempted before the original diff error is returned.

## Tests

- Adjacent changed cells with one style are emitted as one printable text run, rather than separate cursor moves.
- A style change splits the run.
- A final-column change still uses erase rather than printing a glyph.
- The synchronized-update wrapper emits both start and end markers around a successful frame diff.

## Non-goals

- Switching fullscreen history to native terminal scrollback.
- Terminal capability probing or a configuration flag. Unknown terminals safely ignore the synchronization escapes.
- Changing inline renderer behavior.
