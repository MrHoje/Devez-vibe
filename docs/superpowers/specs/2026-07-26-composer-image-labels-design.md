# Composer image labels design

## Goal

When an image path is pasted into the composer, show a compact Claude-style
label such as `[Image #1]` instead of the full path. Sending the turn must
continue to use the original, unmodified text.

## Approach

Apply the transformation only in the composer renderer. The `Editor` buffer,
cursor handling, history, and `AppState::turn_input` retain the pasted path.
`input_lines` receives the raw editor text and produces a display string in
which image-file paths are replaced with numbered labels in appearance order.

## Detection and display rules

- Recognize paths ending in common image extensions (`png`, `jpg`, `jpeg`,
  `gif`, `webp`, `bmp`, `tif`, `tiff`, `avif`), case-insensitively.
- Number recognized paths from one in their order within the current composer:
  `[Image #1]`, `[Image #2]`, and so on.
- Leave non-image paths and all other text unchanged.
- Do not change the submitted text, completion bindings, editor history, or
  the path passed to the app server.

## Cursor behavior

The raw editor remains authoritative for editing and submission. The renderer
maps cursor placement across each collapsed path so the cursor stays at the
start or end of the displayed image label rather than exposing the full path.

## Testing

Add renderer tests proving that a single image path is rendered as `[Image #1]`,
multiple image paths receive consecutive labels, ordinary paths are unchanged,
and the raw editor contents remain available to submission unchanged.
