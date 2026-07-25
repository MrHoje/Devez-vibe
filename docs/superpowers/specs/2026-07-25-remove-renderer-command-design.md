# Remove Renderer Slash Command Design

## Goal

Remove the `/renderer` slash command while preserving startup renderer
configuration through `--renderer`, `DEVEZ_RENDERER`, and the existing saved
`renderer.txt` preference.

## Changes

- Remove `/renderer` from the slash-command catalog and completion panel.
- Remove `/renderer <mode>` parsing and its usage notice.
- Remove the now-unreachable render-mode action, state transition, persistence
  writer, and command-specific tests.
- Keep `RenderMode`, startup parsing, saved-preference loading, fullscreen and
  inline renderer implementations, and CLI/environment configuration unchanged.

## Behavior

Typing `/` no longer shows `/renderer`. Submitting `/renderer` follows the same
unknown-command path as any other unsupported slash command. Existing users keep
their previously saved renderer preference, and can override it at startup.

## Verification

- The command catalog contains no `/renderer` entry.
- `/renderer` and `/renderer inline` do not dispatch a render-mode action.
- `--renderer`, `DEVEZ_RENDERER`, and saved preference parsing continue to pass
  their existing tests.
- The complete test suite and build remain clean.
