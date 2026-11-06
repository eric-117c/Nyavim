---
id: nyavim-syntax
type: module-design
status: draft
parent: nyavim-architecture
title: syntax 模块：行级词法器与跨行状态
---

# syntax 模块

把一行文本变成若干着色区间，并且只记住跨行结构该记住的东西。它不接触终端、光标或缓冲，因此每条词法规则都能单测。

## 契约

- **区间单位是字符列**，不是字节也不是显示格：渲染侧已经按字符遍历，用字符列才不会在非 ASCII 文本上错位。
- 入口只有两个：`highlight_line(language, line, state) -> (tokens, next_state)` 与 `state_after(...)`（只取状态）。
- `Language::detect(path)` 只看扩展名（`Cargo.lock` 特判为 TOML），未知一律 `Plain` —— 没有按内容猜测。
- `State` 只装跨行结构：Rust 的块注释深度（可嵌套）、Markdown 围栏。字符串引号种类、转义都不跨行保存，这是有意的简化。
- 类别固定七种：关键字、字符串、注释、数字、类型、标题、链接。标点不着色，避免满屏花点。

## 有意的限制

- 没有字符串内插（如 Rust 的 `format!` 占位符、Shell 的 `$()` 内部再着色）。
- 原始字符串 `r#"…"#`、字符字面量与生命周期 `'a` 都按普通字符串/符号处理。
- Markdown 只认标题、围栏、行内代码与链接，不处理强调与表格。
- 词法只对可见行执行；跳转超过 2 万行时放弃回推状态，以普通状态起步（在调用方 `App::sync_highlight` 里实施）。
