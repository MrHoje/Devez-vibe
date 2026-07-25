# Keyboard-only prompt history

## Goal

Prompt history must move only when the user presses the keyboard Up or Down
arrow. Spinning the mouse wheel must not recall or replace prompt text.

## Root cause

Fullscreen mode enters the terminal's alternate screen without changing its
alternate-scroll setting. Some terminals translate wheel movement in that mode
into ordinary Up and Down key sequences. Crossterm therefore reports the wheel
as keyboard input, and `AppState::handle_key` correctly—but unintentionally—
routes it to `Editor::history_previous` or `Editor::history_next`.

## Design

When entering fullscreen mode, save the terminal's DEC private mode 1007
(alternate scroll) and disable it. Restore the saved setting when leaving the
fullscreen session. Inline rendering remains unchanged because it uses the
terminal's native scrollback.

Do not enable mouse capture. Capturing the mouse would distinguish wheel events,
but it would also take native terminal mouse shortcuts such as Ctrl+wheel zoom
away from the user. Do not infer wheel input from event timing because genuine
keyboard arrows and terminal-generated arrows are otherwise indistinguishable.

Keyboard handling in `AppState` remains unchanged:

- Up calls `Editor::history_previous`.
- Down calls `Editor::history_next`.
- Shift+arrow remains reserved for fullscreen transcript navigation.

## Testing

Update the terminal-entry unit test to verify that fullscreen entry:

1. enters the alternate screen,
2. saves DEC private mode 1007, and
3. disables DEC private mode 1007 without enabling mouse capture.

Add the matching exit assertion to verify that mode 1007 is restored before
leaving the alternate screen. Existing editor history tests continue to prove
that keyboard Up and Down navigation works.

## Scope

Only fullscreen terminal setup and teardown change. Prompt storage, history
ordering, renderer scrolling, inline mode, and unrelated input behavior are out
of scope.
