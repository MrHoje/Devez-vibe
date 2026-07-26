# Codex Composer Completions Design

## Goal

Match the current Codex TUI's `$` and `@` composer completion behavior while
keeping DevezCLI's existing docked suggestion panel, borders, colors, and
keyboard conventions.

## Behavior

- `$` searches enabled plugins, skills, and apps.
- `@` searches enabled plugins, skills, files, and directories.
- Choosing a skill inserts `$skill-name`.
- Choosing a plugin inserts `@Plugin-Name`.
- Choosing an app inserts `$app-name`.
- Choosing a file or directory removes the `@` query and inserts its relative
  path, quoting paths containing spaces.
- A completion always leaves the cursor after one separating space.
- `Up`/`Down`, `Ctrl+P`/`Ctrl+N`, `Tab`/`Enter`, and `Esc` match Codex composer
  navigation. `Left`/`Right` cycles the `@` search modes: all, filesystem, and
  tools.
- Completion follows the sigil token at the cursor, including tokens in the
  middle of a multiline draft. Email addresses and common uppercase shell
  variables do not open completion.

## Architecture

`src/completion.rs` owns cursor-aware token detection, candidate types,
filtering/ranking, workspace indexing, and insertion metadata. `AppState`
combines that pure completion model with the already loaded app-server skill,
plugin, and app catalogs and routes keys before normal editor history handling.
`renderer.rs` continues to own the DevezCLI dock and gains only dynamic headings,
category labels, and hints.

Workspace entries are indexed once after the thread cwd is known. The `ignore`
walker respects repository ignore files and omits ignored build and VCS content.
An unavailable or unreadable path produces an empty filesystem result while
tool completions remain usable.

## Data and Submission

Selection changes only visible composer text. The existing `turn_input` logic
then resolves `$skill`, `@plugin`, and `$app` tokens into the app-server's typed
`skill` and `mention` items. File and directory selections remain ordinary text
paths, matching Codex.

## Testing

Pure tests cover token targeting, shell/email exclusions, catalog membership,
ranking, modes, and inserted text. State tests cover keyboard navigation,
selection without submission, mid-draft replacement, and typed turn items.
Renderer tests cover the dynamic DevezCLI panel heading and category/hint rows.

