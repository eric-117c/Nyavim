A lightweight terminal text editor: vim's editing model, plus the thing vim is bad at — you never have to remember how to do something.

Every action is searchable by name in the command palette (`Ctrl+K`), the keys that work right now are always on screen, and `F1` lists everything. vim keys still work, so muscle memory keeps paying off.

## Editing

- vim subset — motions (`hjkl w b 0 $ gg G`), `d`/`c`/`y` with motions, `dd`/`cc`/`yy`, counts (`3j`, `2dd`), `p`/`P`, `x`, `J`, `%`, undo/redo
- visual mode — `v` to select, `o` to swap ends, then `d`/`c`/`y`
- text objects — `diw`, `daw`, `ci"`, `da(`, and `i`/`a` with `'`, `` ` ``, `[`, `{`, `<`
- named registers — `"ayy`, `"ap`, alongside the unnamed one
- `.` repeats the last change, including the motion of an operator and the text of an insert session

## Finding things

- incremental `/` (forward) and `?` (backward) search, every match highlighted as you type, `n`/`N` continuing in the search direction, smart case
- `:s/old/new/` and `:%s/old/new/g` literal substitute, with `c` asking `y`/`n`/`a`/`q` per match
- `Ctrl+P` file picker over the project — fuzzy, reusing the palette's matcher
- `Ctrl+K` command palette that searches names **and** descriptions, so "save" finds `write`

## Layout

- tabs, plus window splits (`:sp`, `:vsp`, `Ctrl+W` `hjkl`/`w`/`c`/`o`), each window with its own cursor and scroll
- mouse — click a tab, click text to place the cursor, wheel to scroll
- long lines wrap to the window width (`wrap = false` in the config folds that back)

## Files

- line endings (LF/CRLF) and BOM preserved; saves are atomic, so a crash cannot truncate your file
- syntax highlighting for Rust, TOML, JSON, Markdown and shell, with block comments and code fences carrying across lines
- files over 50 MB open read-only instead of becoming unresponsive
- crash recovery — a dirty buffer is mirrored to a swap file and offered back on the next open

## Tooling

- git — changed-line gutter against HEAD (including unsaved edits), branch in the status line, `:commit` with an inline message
- a real shell on a pty (`Ctrl+T`, back with `Ctrl+\`), so `vim`, `htop` and `less` work inside it
- TOML configuration — tab width, scroll margin, swap interval, read-only threshold, theme colours, file types, **and key bindings** via `[keys]`

## Install

```sh
tar -xzf nyavim-*.tar.gz && ./nyavim <file>     # prebuilt binary
cargo install --git https://github.com/eric-117c/Nyavim
```

## Known gaps

- motions, counts and text objects cannot be rebound (only whole actions can)
- no system clipboard, splits are fixed at 50/50, no `.` for visual selections
- a single line taller than the window cannot be scrolled through row by row
