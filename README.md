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

Early development. The first usable slice (open → edit → save, tabs, command palette, vim subset,
undo/redo) is being built; syntax highlighting, splits, search, git integration, a built-in terminal
and TOML configuration come after.

The product definition lives in [`goal-and-requirements.md`](./goal-and-requirements.md).

## Build

```sh
cargo build --release
./target/release/nyavim <file>
```

Requires a terminal with UTF-8 support on Linux or macOS.
