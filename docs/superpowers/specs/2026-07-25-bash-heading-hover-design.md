# Bash Heading Hover Design

## Goal

Make every fullscreen Bash heading that can be clicked to expand or collapse visibly respond to mouse hover.

## Scope

- Track the Bash tool block under the mouse in fullscreen mode.
- Apply a subtle theme-based background to the visible `Bash · command · exit · duration` text.
- Keep the disclosure arrow and unused row space unchanged.
- Apply the hover style to every wrapped row belonging to the same clickable Bash heading.
- Clear the hover when the pointer leaves the heading.
- Preserve existing click, drag-selection, and scroll behavior.
- Do not add hover styling to pickers, command lists, or non-Bash blocks.
- Inline mode remains unchanged because it does not own mouse interaction.

## Design

`mouse_request` will expose mouse-move coordinates to the renderer. The renderer will resolve the row through the existing `PaintLine::tool_heading` metadata and store the hovered block ID only when that row belongs to an expandable Bash heading.

During fullscreen painting, title spans belonging to the hovered block receive a restrained theme background. The prefix containing `▸` or `▾` is painted normally, so the hover affordance covers the Bash label rather than the disclosure icon. Wrapped heading rows share the same block ID and therefore receive the same treatment.

Changing or clearing the hovered block requests a repaint. Repeated move events inside the same block do not.

## Testing

- Moving onto a Bash heading changes the hovered block and requests repaint.
- Moving outside it clears the hover and requests repaint.
- Moving within the same heading does not request another repaint.
- Hover painting affects the heading text but not the disclosure arrow.
- Existing click-to-toggle and selection tests remain green.
