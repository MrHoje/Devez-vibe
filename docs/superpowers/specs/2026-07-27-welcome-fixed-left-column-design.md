# Welcome panel fixed left column

## Goal

Keep the welcome panel's account and workspace information column at a stable,
readable width while letting release notes use all additional terminal width.

## Layout

- At split-capable widths (inner width of at least 62 cells), the left column is
  exactly 48 cells wide.
- The right column receives the remaining inner width after the one-cell divider.
- Below 62 inner cells, retain the existing single-column panel.
- Existing borders, text wrapping, and content remain unchanged.

## Verification

Add a renderer test that renders a wide panel and asserts the split top border
contains 48 cells to the left of the divider, with all remaining cells assigned
to the notes column. Retain the existing narrow-panel test.
