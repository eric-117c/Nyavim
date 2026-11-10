# Changelog

## 0.2.0

Everything below is the same release; versions were not cut while the editor was being built.

### Editing
- vim subset: motions, `d`/`c`/`y` with motions, `dd`/`cc`/`yy`, counts, `p`/`P`, `x`, `J`, `%`,
  undo/redo with snapshot history
- visual mode (`v`, `o`, `d`/`c`/`y` on the selection)
- text objects: `iw`/`aw`, and `i`/`a` with `"`, `'`, `` ` ``, `(`, `[`, `{`, `<`
- named registers (`"ay`, `"ap`) alongside the unnamed one
- `e` / `E` word-end motions
- `.` repeats the last change (operators with their motion, `x`, `p`, insert sessions with the text
  you typed, and visual selections by width)
- key bindings are data: a `[keys]` table in the config rewrites them
- yanks go to the system clipboard (helper tool, or OSC 52 over ssh); an empty register pastes from it

### Finding things
- incremental `/` (forward) and `?` (backward) search with all matches highlighted, `n`/`N`
  continuing in the search direction, smart case
- `:s` / `:%s` literal substitute with `g` and per-match `c` confirmation
- `Ctrl+P` file picker over the project, and `Ctrl+K` command palette searching names *and*
  descriptions, sharing one registry with the `:`
  command line and the `F1` page

### Layout
- tabs, and window splits (`:sp`, `:vsp`, `Ctrl+W` `hjkl`/`w`/`c`/`o`) with per-window cursors
- mouse: click a tab, click text to place the cursor, wheel to scroll

### Knowing what's going on
- status line: mode, file, position, line ending, language, read-only flag, git branch
- hint line always shows the keys that work right now, including half-typed commands

### Files
- line endings (LF/CRLF) and BOM preserved, atomic saves
- syntax highlighting for Rust, TOML, JSON, Markdown and shell, with block comments and fences
  carrying across lines
- files over 50 MB open read-only instead of becoming unresponsive
- crash recovery: dirty buffers are mirrored to a swap file and offered back on the next open

### Tooling
- git: changed-line gutter against HEAD (including unsaved edits), branch in the status line,
  `:commit` with an inline message
- a real shell on a pty (`Ctrl+T`, back with `Ctrl+\`)
- TOML configuration for tab width, scroll margin, swap interval, read-only threshold, theme colours
  and file types

### Known gaps
- motions, counts and text objects cannot be rebound (only whole actions can)
- wrapping folds long lines by character width; a single line taller than the window still cannot be
  scrolled through row by row
- no `.` (repeat last change); splits are fixed at 50/50 and cannot be dragged
- no file opener beyond `:e <path>`, no system clipboard, splits are fixed at 50/50
