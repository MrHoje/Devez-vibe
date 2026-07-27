# Fixed Vibe presets

## Goal

Replace Custom Vibe mode with three fixed presets.

## Presets

- Vibe: Short, Collapse, Collapse
- Super Vibe: Short, Hide, Hide
- Normal: Short, Expand, Expand

## Interaction

The composer shows only `Vibe mode: <preset>`. Clicking it cycles the three presets and applies all values together. Remove Custom and the `/vibemode` editor panel.

## Verification

Focused state tests cover each preset's three values and the cycle order.
