//! Hand-written, line-at-a-time highlighting.
//!
//! A real parser would be more accurate, but a lexer per line keeps the editor light and makes
//! highlighting cost proportional to what is on screen. Ranges are character columns so they line up
//! with the cursor model; the renderer never has to convert bytes.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Plain,
    Rust,
    Toml,
    Json,
    Markdown,
    Shell,
}

impl Language {
    pub fn detect(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Language::Plain;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let extension = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        match extension.as_str() {
            "rs" => Language::Rust,
            "toml" => Language::Toml,
            "json" | "jsonc" => Language::Json,
            "md" | "markdown" => Language::Markdown,
            "sh" | "bash" | "zsh" => Language::Shell,
            _ if name == "cargo.lock" => Language::Toml,
            _ => Language::Plain,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Language::Plain => "TEXT",
            Language::Rust => "RUST",
            Language::Toml => "TOML",
            Language::Json => "JSON",
            Language::Markdown => "MARKDOWN",
            Language::Shell => "SHELL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    /// Also covers shell variables and inline code.
    String,
    Comment,
    Number,
    Type,
    Heading,
    Link,
}

/// What survives from one line to the next: block comments (nested in Rust) and Markdown fences.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    pub block_depth: u16,
    pub in_fence: bool,
}

/// A styled range of one line, in character columns.
pub type Token = (usize, usize, TokenKind);

/// Tokenize one line, returning its ranges and the state the next line starts in.
pub fn highlight_line(language: Language, line: &str, state: State) -> (Vec<Token>, State) {
    let chars: Vec<char> = line.chars().collect();
    let mut scanner = Scanner {
        chars,
        tokens: Vec::new(),
        state,
        language,
    };
    scanner.run();
    (scanner.tokens, scanner.state)
}

/// State after `line`, without keeping the tokens.
pub fn state_after(language: Language, line: &str, state: State) -> State {
    highlight_line(language, line, state).1
}

struct Scanner {
    chars: Vec<char>,
    tokens: Vec<Token>,
    state: State,
    language: Language,
}

impl Scanner {
    fn run(&mut self) {
        match self.language {
            Language::Plain => {}
            Language::Rust => self.rust(),
            Language::Toml => self.toml(),
            Language::Json => self.json(),
            Language::Markdown => self.markdown(),
            Language::Shell => self.shell(),
        }
    }

    fn len(&self) -> usize {
        self.chars.len()
    }

    fn at(&self, index: usize) -> Option<char> {
        self.chars.get(index).copied()
    }

    fn starts_with(&self, index: usize, needle: &str) -> bool {
        needle
            .chars()
            .enumerate()
            .all(|(offset, expected)| self.at(index + offset) == Some(expected))
    }

    fn push(&mut self, start: usize, end: usize, kind: TokenKind) {
        if start < end {
            self.tokens.push((start, end, kind));
        }
    }

    /// Advance past a single-line string starting at `start`; returns the index after it.
    fn skip_string(&self, start: usize, quote: char) -> usize {
        let mut index = start + 1;
        while index < self.len() {
            match self.chars[index] {
                '\\' => index += 2,
                ch if ch == quote => return index + 1,
                _ => index += 1,
            }
        }
        self.len()
    }

    /// Number starting at `start`: digits, hex, underscores, decimals and suffixes.
    fn number_end(&self, start: usize) -> usize {
        let mut index = start;
        while index < self.len() {
            let ch = self.chars[index];
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
                index += 1;
            } else {
                break;
            }
        }
        index
    }

    fn identifier_end(&self, start: usize) -> usize {
        let mut index = start;
        while index < self.len() {
            let ch = self.chars[index];
            if ch.is_alphanumeric() || ch == '_' {
                index += 1;
            } else {
                break;
            }
        }
        index
    }

    /// Classify an identifier as keyword/type/link, or nothing.
    fn word(&mut self, start: usize, end: usize, keywords: &[&str]) {
        let word: String = self.chars[start..end].iter().collect();
        if keywords.contains(&word.as_str()) {
            self.push(start, end, TokenKind::Keyword);
        } else if word.chars().next().is_some_and(|ch| ch.is_uppercase()) {
            self.push(start, end, TokenKind::Type);
        }
    }

    // ---- Rust ----------------------------------------------------------

    fn rust(&mut self) {
        const KEYWORDS: &[&str] = &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
            "trait", "true", "type", "unsafe", "use", "where", "while",
        ];

        let mut index = 0;
        while index < self.len() {
            // inside a block comment that started on an earlier line
            if self.state.block_depth > 0 {
                let start = index;
                while index < self.len() {
                    if self.starts_with(index, "/*") {
                        self.state.block_depth += 1;
                        index += 2;
                    } else if self.starts_with(index, "*/") {
                        self.state.block_depth -= 1;
                        index += 2;
                        if self.state.block_depth == 0 {
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
                self.push(start, index, TokenKind::Comment);
                continue;
            }

            let ch = self.chars[index];
            if self.starts_with(index, "//") {
                self.push(index, self.len(), TokenKind::Comment);
                return;
            }
            if self.starts_with(index, "/*") {
                let start = index;
                self.state.block_depth = 1;
                index += 2;
                while index < self.len() && self.state.block_depth > 0 {
                    if self.starts_with(index, "/*") {
                        self.state.block_depth += 1;
                        index += 2;
                    } else if self.starts_with(index, "*/") {
                        self.state.block_depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                self.push(start, index, TokenKind::Comment);
                continue;
            }
            if ch == '"' || ch == '\'' {
                let end = self.skip_string(index, ch);
                self.push(index, end, TokenKind::String);
                index = end;
                continue;
            }
            if ch.is_ascii_digit() {
                let end = self.number_end(index);
                self.push(index, end, TokenKind::Number);
                index = end;
                continue;
            }
            if ch.is_alphabetic() || ch == '_' {
                let end = self.identifier_end(index);
                self.word(index, end, KEYWORDS);
                index = end;
                continue;
            }
            index += 1;
        }
    }

    // ---- TOML ----------------------------------------------------------

    fn toml(&mut self) {
        const KEYWORDS: &[&str] = &["true", "false"];
        let mut index = 0;
        while index < self.len() {
            let ch = self.chars[index];
            if ch == '#' {
                self.push(index, self.len(), TokenKind::Comment);
                return;
            }
            if ch == '"' || ch == '\'' {
                let end = self.skip_string(index, ch);
                self.push(index, end, TokenKind::String);
                index = end;
                continue;
            }
            if ch == '[' {
                let end = self
                    .chars
                    .iter()
                    .skip(index)
                    .position(|c| *c == ']')
                    .map(|offset| index + offset + 1)
                    .unwrap_or(self.len());
                self.push(index, end, TokenKind::Heading);
                index = end;
                continue;
            }
            if ch.is_ascii_digit() {
                let end = self.number_end(index);
                self.push(index, end, TokenKind::Number);
                index = end;
                continue;
            }
            if ch.is_alphabetic() || ch == '_' {
                let end = self.identifier_end(index);
                // a bare word is a key unless it is a boolean
                let word: String = self.chars[index..end].iter().collect();
                if KEYWORDS.contains(&word.as_str()) {
                    self.push(index, end, TokenKind::Keyword);
                } else if self.chars[end..].iter().find(|c| **c != ' ') == Some(&'=') {
                    self.push(index, end, TokenKind::Type);
                }
                index = end;
                continue;
            }
            index += 1;
        }
    }

    // ---- JSON ----------------------------------------------------------

    fn json(&mut self) {
        const KEYWORDS: &[&str] = &["true", "false", "null"];
        let mut index = 0;
        while index < self.len() {
            let ch = self.chars[index];
            if ch == '"' {
                let end = self.skip_string(index, ch);
                // a string used as a key reads as a type
                let is_key = self.chars[end..].iter().find(|c| !c.is_whitespace()) == Some(&':');
                self.push(
                    index,
                    end,
                    if is_key {
                        TokenKind::Type
                    } else {
                        TokenKind::String
                    },
                );
                index = end;
                continue;
            }
            if ch.is_ascii_digit()
                || (ch == '-' && self.at(index + 1).is_some_and(|c| c.is_ascii_digit()))
            {
                let end = self.number_end(index);
                self.push(index, end, TokenKind::Number);
                index = end;
                continue;
            }
            if ch.is_alphabetic() {
                let end = self.identifier_end(index);
                self.word(index, end, KEYWORDS);
                index = end;
                continue;
            }
            index += 1;
        }
    }

    // ---- Markdown ------------------------------------------------------

    fn markdown(&mut self) {
        let trimmed_start = self.chars.iter().take_while(|c| **c == ' ').count();

        // fenced code blocks keep their own state
        if self.starts_with(trimmed_start, "```") {
            self.push(trimmed_start, self.len(), TokenKind::Comment);
            self.state.in_fence = !self.state.in_fence;
            return;
        }
        if self.state.in_fence {
            self.push(0, self.len(), TokenKind::String);
            return;
        }

        if self.at(trimmed_start) == Some('#') {
            self.push(trimmed_start, self.len(), TokenKind::Heading);
            return;
        }

        let mut index = 0;
        while index < self.len() {
            if self.starts_with(index, "](") {
                // link target: [label](target)
                let end = self.chars[index..]
                    .iter()
                    .position(|c| *c == ')')
                    .map(|offset| index + offset + 1)
                    .unwrap_or(self.len());
                self.push(index, end, TokenKind::Link);
                index = end;
                continue;
            }
            if self.at(index) == Some('`') {
                let end = self.chars[index + 1..]
                    .iter()
                    .position(|c| *c == '`')
                    .map(|offset| index + offset + 2)
                    .unwrap_or(self.len());
                self.push(index, end, TokenKind::String);
                index = end;
                continue;
            }
            index += 1;
        }
    }

    // ---- Shell ---------------------------------------------------------

    fn shell(&mut self) {
        const KEYWORDS: &[&str] = &[
            "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in",
            "local", "return", "then", "while", "export",
        ];
        let mut index = 0;
        while index < self.len() {
            let ch = self.chars[index];
            if ch == '#' && (index == 0 || self.chars[index - 1].is_whitespace()) {
                self.push(index, self.len(), TokenKind::Comment);
                return;
            }
            if ch == '"' || ch == '\'' {
                let end = self.skip_string(index, ch);
                self.push(index, end, TokenKind::String);
                index = end;
                continue;
            }
            if ch == '$' {
                let end = self.identifier_end(index + 1);
                self.push(index, end.max(index + 1), TokenKind::String);
                index = end.max(index + 1);
                continue;
            }
            if ch.is_ascii_digit() {
                let end = self.number_end(index);
                self.push(index, end, TokenKind::Number);
                index = end;
                continue;
            }
            if ch.is_alphabetic() || ch == '_' {
                let end = self.identifier_end(index);
                let word: String = self.chars[index..end].iter().collect();
                if KEYWORDS.contains(&word.as_str()) {
                    self.push(index, end, TokenKind::Keyword);
                } else if self.chars[end..].iter().find(|c| **c != ' ') == Some(&'(') {
                    self.push(index, end, TokenKind::Type);
                }
                index = end;
                continue;
            }
            index += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(language: Language, line: &str) -> Vec<(String, TokenKind)> {
        let (tokens, _) = highlight_line(language, line, State::default());
        tokens
            .into_iter()
            .map(|(start, end, kind)| {
                let text: String = line.chars().skip(start).take(end - start).collect();
                (text, kind)
            })
            .collect()
    }

    #[test]
    fn language_is_detected_from_the_extension() {
        assert_eq!(Language::detect(Some(Path::new("a/b.rs"))), Language::Rust);
        assert_eq!(
            Language::detect(Some(Path::new("Cargo.toml"))),
            Language::Toml
        );
        assert_eq!(
            Language::detect(Some(Path::new("Cargo.lock"))),
            Language::Toml
        );
        assert_eq!(Language::detect(Some(Path::new("x.json"))), Language::Json);
        assert_eq!(
            Language::detect(Some(Path::new("README.md"))),
            Language::Markdown
        );
        assert_eq!(Language::detect(Some(Path::new("run.sh"))), Language::Shell);
        assert_eq!(
            Language::detect(Some(Path::new("notes.txt"))),
            Language::Plain
        );
        assert_eq!(Language::detect(None), Language::Plain);
    }

    #[test]
    fn rust_keywords_strings_numbers_and_types() {
        let tokens = kinds(Language::Rust, "let x: Foo = \"hi\" + 42;");
        assert!(tokens.contains(&("let".to_owned(), TokenKind::Keyword)));
        assert!(tokens.contains(&("\"hi\"".to_owned(), TokenKind::String)));
        assert!(tokens.contains(&("42".to_owned(), TokenKind::Number)));
        assert!(tokens.contains(&("Foo".to_owned(), TokenKind::Type)));
    }

    #[test]
    fn rust_block_comments_carry_across_lines() {
        let (tokens, state) = highlight_line(Language::Rust, "/* start", State::default());
        assert_eq!(tokens[0].2, TokenKind::Comment);
        assert_eq!(state.block_depth, 1);

        let (tokens, state) = highlight_line(Language::Rust, " still commented", state);
        assert_eq!(tokens, vec![(0, 16, TokenKind::Comment)]);
        assert_eq!(state.block_depth, 1);

        let (tokens, state) = highlight_line(Language::Rust, " end */ let x = 1;", state);
        assert_eq!(tokens[0].2, TokenKind::Comment);
        assert_eq!(state.block_depth, 0);
        assert!(tokens.iter().any(|t| t.2 == TokenKind::Keyword));
    }

    #[test]
    fn rust_nested_block_comments_do_not_end_early() {
        let (_, state) = highlight_line(Language::Rust, "/* a /* b */", State::default());
        assert_eq!(
            state.block_depth, 1,
            "the inner close only ends the inner block"
        );
        let (tokens, state) = highlight_line(Language::Rust, " */", state);
        assert_eq!(state.block_depth, 0);
        assert_eq!(tokens[0].2, TokenKind::Comment);
    }

    #[test]
    fn a_string_containing_comment_markers_is_not_a_comment() {
        let tokens = kinds(Language::Rust, "let s = \"// not a comment\";");
        assert!(tokens.contains(&("\"// not a comment\"".to_owned(), TokenKind::String)));
        assert!(!tokens.iter().any(|(_, kind)| *kind == TokenKind::Comment));
    }

    #[test]
    fn toml_headers_keys_and_booleans() {
        let tokens = kinds(Language::Toml, "[section]");
        assert_eq!(tokens[0].1, TokenKind::Heading);

        let tokens = kinds(Language::Toml, "name = \"nyavim\" # comment");
        assert!(tokens.contains(&("name".to_owned(), TokenKind::Type)));
        assert!(tokens.contains(&("\"nyavim\"".to_owned(), TokenKind::String)));
        assert!(tokens.contains(&("# comment".to_owned(), TokenKind::Comment)));

        let tokens = kinds(Language::Toml, "enabled = true");
        assert!(tokens.contains(&("true".to_owned(), TokenKind::Keyword)));
    }

    #[test]
    fn json_keys_and_values_are_distinguished() {
        let tokens = kinds(Language::Json, "\"key\": \"value\", \"n\": null");
        assert!(tokens.contains(&("\"key\"".to_owned(), TokenKind::Type)));
        assert!(tokens.contains(&("\"value\"".to_owned(), TokenKind::String)));
        assert!(tokens.contains(&("null".to_owned(), TokenKind::Keyword)));
    }

    #[test]
    fn markdown_headings_inline_code_and_fences() {
        let tokens = kinds(Language::Markdown, "## Title");
        assert_eq!(tokens[0].1, TokenKind::Heading);

        let tokens = kinds(Language::Markdown, "run `cargo test` now");
        assert!(tokens.contains(&("`cargo test`".to_owned(), TokenKind::String)));

        let (_, state) = highlight_line(Language::Markdown, "```rust", State::default());
        assert!(state.in_fence);
        let (tokens, _) = highlight_line(Language::Markdown, "let x = 1;", state);
        assert_eq!(tokens[0].2, TokenKind::String, "fenced lines read as code");
        let (_, state) = highlight_line(
            Language::Markdown,
            "```",
            State {
                in_fence: true,
                ..State::default()
            },
        );
        assert!(!state.in_fence);
    }

    #[test]
    fn shell_comments_variables_and_keywords() {
        let tokens = kinds(Language::Shell, "if [ -f \"$file\" ]; then # check");
        assert!(tokens.contains(&("if".to_owned(), TokenKind::Keyword)));
        assert!(tokens.contains(&("\"$file\"".to_owned(), TokenKind::String)));
        assert!(tokens.contains(&("# check".to_owned(), TokenKind::Comment)));

        let tokens = kinds(Language::Shell, "echo $HOME");
        assert!(tokens.contains(&("$HOME".to_owned(), TokenKind::String)));
    }

    #[test]
    fn plain_text_stays_unstyled() {
        assert!(kinds(Language::Plain, "let x = 1; // fake").is_empty());
    }

    #[test]
    fn ranges_are_character_columns_not_bytes() {
        // "é" is two bytes: a byte-based implementation would report a shifted column
        let tokens = kinds(Language::Rust, "let é = 1;");
        assert_eq!(tokens[0], ("let".to_owned(), TokenKind::Keyword));
        let (pos, end, kind) = highlight_line(Language::Rust, "let é = 1;", State::default()).0[1];
        assert_eq!(kind, TokenKind::Number);
        assert_eq!((pos, end), (8, 9), "the 1 sits at column 8");
    }

    #[test]
    fn state_after_matches_highlight_line() {
        let (_, state) = highlight_line(Language::Rust, "/* open", State::default());
        assert_eq!(
            state_after(Language::Rust, "/* open", State::default()),
            state
        );
    }
}
