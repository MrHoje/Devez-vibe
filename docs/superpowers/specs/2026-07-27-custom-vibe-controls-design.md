# Custom Vibe controls

## Goal

Expose the original Response, Shell, and Diff composer controls only while Vibe mode is Custom.

## Behavior

Vibe and Super Vibe show only the Vibe mode badge. Custom replaces that badge with Response, Shell, and Diff controls, each retaining its existing click behavior. Clicking any Custom control keeps the mode Custom even when its resulting values match a preset.

Selecting Vibe mode from the Custom control path returns to the preset cycle.

## Verification

Focused renderer tests cover the visible Custom labels and their click picks.
