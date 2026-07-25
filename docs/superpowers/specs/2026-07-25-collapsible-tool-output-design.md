# Collapsible Tool Output Design

## Goal

Keep command results compact by default. A completed or active Bash block shows
only its command heading and available status metadata. In fullscreen mode, a
left click on that heading expands or collapses that block's output.

## Interaction

- Collapsed: `▸ Bash · <command> · exit <code> · <duration>`
- Expanded: `▾ Bash · <command> · exit <code> · <duration>`, followed by the
  complete non-empty output.
- Each block toggles independently.
- Only a left click on the heading toggles the block. Wheel scrolling and clicks
  elsewhere keep their current behavior.
- New Bash blocks start collapsed.
- Inline mode keeps Bash blocks collapsed because previously printed terminal
  scrollback cannot be retracted safely and mouse capture is intentionally off.
- Non-Bash tool, file-change, reasoning, and message blocks are unchanged.

## Architecture

The renderer owns transient presentation state:

- Give each rendered block a stable renderer-local identity.
- Store the identities of expanded Bash blocks in `Renderer`.
- While composing a fullscreen frame, record the visible screen-row range and
  block identity for each clickable Bash heading.
- On a left-click event, resolve the screen row through the renderer hit map,
  toggle the matching identity, rewrap the transcript, and request a redraw.

The application state and protocol payloads remain unchanged. Expansion is a
local display preference and does not affect session history.

## Rendering

`tool_lines` renders the heading in both states. It emits no body rows when
collapsed. When expanded, it renders every non-empty output row with the current
muted tool-output styling and normal terminal-width wrapping. The existing
five-row tail preview and hidden-line counter are removed for Bash output.

If terminal resizing or scrolling changes row positions, the hit map is rebuilt
from the newly composed fullscreen frame before the next click is handled.

## Edge Cases

- Empty output: the heading still toggles visually but adds no rows.
- Long headings: every wrapped heading row is clickable.
- Off-screen headings: absent from the hit map and cannot be toggled until
  visible.
- New output while scrolled up: existing scroll anchoring remains in effect.
- Unknown exit code or duration: omitted exactly as today.

## Tests

- A Bash block renders only its heading while collapsed.
- An expanded Bash block renders all non-empty output rows.
- Heading markers reflect collapsed and expanded state.
- Hit testing toggles only the clicked Bash block.
- Clicking output, composer, or blank rows does nothing.
- Rewrapping after resize preserves expansion identities.
- Inline mode remains collapsed and does not capture click events.

