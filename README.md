# nyavim

A lightweight terminal text editor: vim's editing model, without having to remember it.

Every action is searchable by name in the command palette (`Ctrl+K`), the keys available right now are
always on screen, and `F1` lists everything. vim keys still work, so muscle memory keeps paying off.

```
┌ nyavim ─ src/main.rs ─────────────────────┐
│ fn main() {                               │
│     println!("hi");                       │
│ }                                         │
├───────────────────────────────────────────┤
│ > save                                    │
│   Save file                     Ctrl+S    │
│   Save as…                      Ctrl+Sh+S │
│   Reopen with encoding…                   │
└ NORMAL   Ctrl+K palette   F1 help         ┘
```

Written in Rust: one static binary, no runtime, no plugin runtime.

## Status

Usable for real editing today:

- vim editing subset — motions, `d`/`c`/`y` with motions, `dd`/`cc`/`yy`, `p`/`P`, counts, insert
  mode, undo/redo, `J`, `%`
- visual mode and text objects (`diw`, `ci"`, `da(`), named registers (`"ay`, `"ap`)
- command palette (`Ctrl+K`) and `:` command line sharing one registry, plus `F1` help
- tabs and window splits (`:sp`, `:vsp`, `Ctrl+W` + `hjkl`)
- incremental `/` and `?` search with match highlighting, `n`/`N`
- `:s` / `:%s` literal substitute with `g` and `c` confirm
- syntax highlighting for Rust, TOML, JSON, Markdown and shell (block comments and code fences carry
  across lines)
- git integration: changed-line gutter, branch in the status line, `:commit` with an inline message
- a real shell on a pty, one keystroke away (`Ctrl+T`, back with `Ctrl+\`)
- crash recovery: dirty buffers are mirrored to a swap file, offered back on the next open
- mouse: click tabs to switch, click text to place the cursor, wheel to scroll
- `Ctrl+P` picks a file from the project (fuzzy, reuses the palette matcher)
- yanks reach the system clipboard: a helper (`wl-copy`, `xclip`, `pbcopy`) when present, otherwise
  OSC 52 so it also works over ssh
- long lines wrap to the window width (`wrap` in the config turns it off)
- files over 50 MB open read-only instead of becoming unresponsive
- line endings and BOM preserved, atomic saves

## Configuration

Optional, read from `~/.config/nyavim/config.toml` (or `$NYAVIM_CONFIG`). Everything has a default
and a broken file only produces a warning.

```toml
[editor]
tab_width = 4          # 1-16
scroll_margin = 2      # lines kept around the cursor
swap_seconds = 2       # how often a dirty buffer is mirrored
read_only_mb = 50      # bigger files open read-only

[theme]                # ANSI names or 0-255 indices
keyword = "magenta"
string = "green"
comment = "darkgray"
number = "yellow"
type = "cyan"
heading = "cyan"
link = "blue"

[filetypes]            # extension -> language, overrides the built-in detection
py = "rust"
```

Key bindings are configurable, motions included: `"L" = "motion-word-forward"`. Counts and text
objects stay fixed.

```toml
[keys]
"Ctrl+E" = "palette"
"Q" = "quit"
```

Run names: every line of the `Actions` table on the `F1` page.

Other known gaps: no system clipboard, and splits are fixed at 50/50 with no dragging.

The product definition lives in [`goal-and-requirements.md`](./goal-and-requirements.md).

## Install

Prebuilt binaries for Linux (x86_64) and macOS (aarch64) are attached to each
[release](https://github.com/eric-117c/Nyavim/releases).

```sh
cargo install --path .      # or: cargo install --git https://github.com/eric-117c/Nyavim
nyavim <file>
```

From a checkout:

```sh
cargo build --release
./target/release/nyavim <file>
```

Requires a terminal with UTF-8 support on Linux or macOS.

## License

MIT — see [LICENSE](./LICENSE).
