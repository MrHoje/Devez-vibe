# Prompt submission follows the latest transcript

## Scope

When a user submits or steers a non-command prompt while the fullscreen renderer
is scrolled into transcript history, return the transcript view to its newest
position. While a fullscreen view is away from the newest position, show a
clickable `Scroll to bottom` control above the composer. Inline mode remains
unchanged because its scrollback is terminal-owned.

## Design

Add a small renderer method that resets fullscreen `scroll_back` to zero. Invoke
it when `Action::Submit` or `Action::Steer` is executed, before the subsequent
redraw. Add a `Pick` for `Scroll to bottom`; the control uses the reserved row
above the composer, appears only while fullscreen `scroll_back` is nonzero, and
calls the same renderer method. Do not call it for streamed server output, so
manually reviewing older content is not interrupted.

## Verification

Add renderer unit tests covering fullscreen reset, inline no-op, and control
visibility/click targeting. Add a main-loop test mapping the control pick to its
local action, then run focused tests and the full Rust test suite.
