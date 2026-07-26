# Prompt submission follows the latest transcript

## Scope

When a user submits or steers a non-command prompt while the fullscreen renderer
is scrolled into transcript history, return the transcript view to its newest
position. Inline mode remains unchanged because its scrollback is terminal-owned.

## Design

Add a small renderer method that resets fullscreen `scroll_back` to zero. Invoke
it when `Action::Submit` or `Action::Steer` is executed, before the subsequent
redraw. Do not call it for streamed server output, so manually reviewing older
content is not interrupted.

## Verification

Add renderer unit tests covering fullscreen reset and inline no-op behavior, then
run the focused test and the full Rust test suite.
