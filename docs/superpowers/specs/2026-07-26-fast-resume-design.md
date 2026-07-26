# Fast Resume Design

## Goal

Make a resumed session accept its first prompt as soon as `thread/resume` returns, without reading local rollout JSONL files or rebuilding historical Bash output.

## Scope

- Do not load or parse `%CODEX_HOME%/sessions/**/rollout-*.jsonl` during startup resume or `/resume`.
- Rebuild the visible transcript only from the `thread/resume` response.
- Preserve the server-provided conversation, model, effort, cwd, and normal turn behavior.
- Historical shell commands, shell output, exit codes, durations, and rollout-only reasoning remain absent after resume.
- Do not block first input on skills, plugin, or app catalogue refresh. Refresh those catalogues from the event loop after the session becomes interactive.

## Design

`start_session` and `resume_into_state` will stop creating and awaiting rollout-load tasks. Both paths will call `AppState::load_history` with no rollout, which preserves the server transcript while avoiding the local JSONL scan, read, parse, and merge work.

The event loop will own the initial integration refresh as a background future. It will continue servicing keyboard input, server events, redraw ticks, and workspace indexing while the skills, plugins, and apps requests resolve. When the future completes, it updates `AppState` or reports its existing error notice.

## Error Handling

Catalogue-refresh failures stay non-fatal: the session remains interactive and the existing notice behavior reports the failure. Since rollout restoration is no longer attempted, missing or malformed rollout files cannot delay or fail resume.

## Tests

- Verify a resumed history loaded without a rollout contains server items but no shell blocks.
- Verify the startup and in-session resume paths do not create a rollout-load task.
- Verify an integration refresh can complete from the event loop without delaying session readiness.
