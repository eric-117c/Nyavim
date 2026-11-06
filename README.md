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
  mode, undo/redo
- command palette (`Ctrl+K`) and `:` command line sharing one registry, plus `F1` help
- tabs and window splits (`:sp`, `:vsp`, `Ctrl+W` + `hjkl`)
- incremental `/` search with match highlighting, `n`/`N`
- `:s` / `:%s` literal substitute with `g` and `c` confirm
- line endings and BOM preserved, atomic saves

Still to come: syntax highlighting, git integration, a built-in terminal, TOML configuration, crash
recovery (swap files), and the >50 MB read-only mode.

The product definition lives in [`goal-and-requirements.md`](./goal-and-requirements.md).

## Build

```sh
cargo build --release
./target/release/nyavim <file>
```

Requires a terminal with UTF-8 support on Linux or macOS.
