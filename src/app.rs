//! Application state and key routing. Rendering lives in [`crate::ui`]; text and files live in
//! [`crate::document`]. Nothing here touches the terminal, so the whole thing is testable without a
//! tty.

use std::path::Path;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::command::{self, Command};
use crate::document::{is_case_sensitive, ordered, Document, LoadError, Pos};
use crate::mode::Mode;

/// What the main loop should do after a key has been handled.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Delete,
    Change,
    Yank,
}

/// The search in flight or the last one, kept so `n`/`N` and the highlight keep working after the
/// prompt closes.
#[derive(Debug, Clone)]
struct Search {
    query: String,
    origin: Pos,
    origin_scroll: usize,
}

/// How many matches the status line bothers to count before it says "and more".
const MATCH_COUNT_CAP: usize = 5000;

/// The unnamed register plus how it was filled, which decides what `p` does.
#[derive(Debug, Clone)]
struct Register {
    text: String,
    linewise: bool,
}

/// Keys typed so far that are waiting for the rest of a command, e.g. the `3` of `3dd` or the `d`
/// of `dw`.
#[derive(Debug, Default)]
struct InputState {
    count: Option<usize>,
    operator: Option<Operator>,
    g_prefix: bool,
}

impl InputState {
    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1).max(1)
    }

    fn push_digit(&mut self, digit: usize) {
        let n = self.count.unwrap_or(0).saturating_mul(10).saturating_add(digit);
        self.count = Some(n);
    }
}

pub struct App {
    mode: Mode,
    /// Message shown next to the mode indicator, e.g. a refused save.
    message: Option<String>,
    buffers: Vec<Document>,
    active: usize,
    input: InputState,
    register: Option<Register>,
    /// First buffer line drawn on screen; kept by [`App::follow_cursor`].
    scroll: usize,
    /// Whether the current insert session has taken its undo checkpoint yet.
    insert_started: bool,
    search: Option<Search>,
    /// Text typed after `:`, without the colon.
    command_line: String,
    /// Text typed into the palette, and which of the matches is selected.
    palette_query: String,
    palette_selected: usize,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            mode: Mode::Normal,
            message: None,
            buffers: vec![Document::empty()],
            active: 0,
            input: InputState::default(),
            register: None,
            scroll: 0,
            insert_started: false,
            search: None,
            command_line: String::new(),
            palette_query: String::new(),
            palette_selected: 0,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn document(&self) -> &Document {
        &self.buffers[self.active]
    }

    fn doc_mut(&mut self) -> &mut Document {
        &mut self.buffers[self.active]
    }

    /// Every open buffer, in tab order.
    pub fn buffers(&self) -> &[Document] {
        &self.buffers
    }

    pub fn active(&self) -> usize {
        self.active
    }

    fn switch_to(&mut self, idx: usize) {
        if idx < self.buffers.len() {
            self.active = idx;
            self.input = InputState::default();
            self.scroll = 0;
        }
    }

    /// Show the next buffer, wrapping around.
    pub fn next_buffer(&mut self) {
        if self.buffers.len() > 1 {
            let next = (self.active + 1) % self.buffers.len();
            self.switch_to(next);
        } else {
            self.message = Some("only one buffer open".to_owned());
        }
    }

    /// Show the previous buffer, wrapping around.
    pub fn prev_buffer(&mut self) {
        if self.buffers.len() > 1 {
            let prev = (self.active + self.buffers.len() - 1) % self.buffers.len();
            self.switch_to(prev);
        } else {
            self.message = Some("only one buffer open".to_owned());
        }
    }

    /// Close the active buffer, refusing when it has unsaved changes.
    pub fn close_buffer(&mut self) {
        if self.document().is_dirty() {
            self.message = Some(format!(
                "\"{}\" has unsaved changes — :w first",
                self.document().display_name()
            ));
            return;
        }
        if self.buffers.len() == 1 {
            self.buffers[0] = Document::empty();
            self.active = 0;
        } else {
            self.buffers.remove(self.active);
            self.active = self.active.min(self.buffers.len() - 1);
        }
        self.input = InputState::default();
        self.scroll = 0;
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn command_line(&self) -> &str {
        &self.command_line
    }

    /// The query to highlight in the buffer, if a search is active or was just confirmed.
    pub fn search_highlight(&self) -> Option<(&str, bool)> {
        let search = self.search.as_ref()?;
        if search.query.is_empty() {
            return None;
        }
        Some((&search.query, is_case_sensitive(&search.query)))
    }

    /// Query text for the `/` prompt row.
    pub fn search_query(&self) -> &str {
        self.search.as_ref().map(|s| s.query.as_str()).unwrap_or("")
    }

    /// `(index of the match under the cursor, total, whether counting stopped early)`.
    pub fn search_progress(&self) -> Option<(usize, usize, bool)> {
        let search = self.search.as_ref()?;
        if search.query.is_empty() {
            return None;
        }
        let case_sensitive = is_case_sensitive(&search.query);
        let cursor = self.document().cursor();
        let mut index = 0;
        let mut total = 0;
        let mut capped = false;

        'lines: for line in 0..self.document().line_count() {
            for col in self.document().find_all_in_line(line, &search.query, case_sensitive) {
                total += 1;
                if Pos::new(line, col) == cursor {
                    index = total;
                }
                if total >= MATCH_COUNT_CAP {
                    capped = true;
                    break 'lines;
                }
            }
        }
        Some((index, total, capped))
    }

    pub fn palette_query(&self) -> &str {
        &self.palette_query
    }

    pub fn palette_selected(&self) -> usize {
        self.palette_selected
    }

    /// Commands matching what has been typed into the palette, best first.
    pub fn palette_matches(&self) -> Vec<&'static Command> {
        command::search(&self.palette_query)
    }

    fn open_palette(&mut self) {
        self.palette_query.clear();
        self.palette_selected = 0;
        self.mode = Mode::Palette;
    }

    fn open_command_line(&mut self) {
        self.command_line.clear();
        self.mode = Mode::Command;
    }

    /// Keys typed so far that the next keystroke will complete, e.g. `2d`. Shown in the hint line so
    /// a half-typed command is never invisible.
    pub fn pending_keys(&self) -> String {
        let mut out = String::new();
        if let Some(n) = self.input.count {
            out.push_str(&n.to_string());
        }
        if let Some(op) = self.input.operator {
            out.push(match op {
                Operator::Delete => 'd',
                Operator::Change => 'c',
                Operator::Yank => 'y',
            });
        }
        if self.input.g_prefix {
            out.push('g');
        }
        out
    }

    /// Keep the cursor inside the visible window, scrolling as little as possible.
    pub fn follow_cursor(&mut self, visible: usize) {
        if visible == 0 {
            return;
        }
        let line = self.document().cursor().line;
        let margin = 2.min(visible / 2);
        if line < self.scroll + margin {
            self.scroll = line.saturating_sub(margin);
        } else if line + margin >= self.scroll + visible {
            self.scroll = (line + margin + 1).saturating_sub(visible);
        }
    }

    /// Open `path` in its own buffer and show it, switching to the existing tab when the file is
    /// already open. A file that does not exist yet opens as an empty buffer that will be created on
    /// the first save. A failure to read one is reported in the status line, not to stderr, so the
    /// user keeps an editor instead of a closed terminal.
    pub fn open(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref();

        if let Some(idx) = self
            .buffers
            .iter()
            .position(|doc| doc.path().is_some_and(|open| open == path))
        {
            self.switch_to(idx);
            return;
        }

        let doc = if path.exists() {
            match Document::open(path) {
                Ok(doc) => doc,
                Err(err) => {
                    self.message = Some(match err {
                        LoadError::NotUtf8 => format!("{}: not UTF-8 text", path.display()),
                        LoadError::Io(err) => format!("{}: {err}", path.display()),
                    });
                    return;
                }
            }
        } else {
            Document::new_at(path)
        };

        if self.is_blank_scratch() {
            self.buffers[self.active] = doc;
        } else {
            self.buffers.push(doc);
            self.active = self.buffers.len() - 1;
        }
        self.input = InputState::default();
        self.scroll = 0;
    }

    /// True when the active buffer is the untouched `[No Name]` start-up buffer, which a newly
    /// opened file should replace rather than sit next to.
    fn is_blank_scratch(&self) -> bool {
        self.buffers.len() == 1
            && self.buffers[0].path().is_none()
            && !self.buffers[0].is_dirty()
            && self.buffers[0].text().is_empty()
    }

    pub fn save(&mut self) {
        let result = self.doc_mut().save();
        self.message = match result {
            Ok(()) => Some(format!("\"{}\" written", self.document().display_name())),
            Err(err) => Some(format!("save failed: {err}")),
        };
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Help => self.handle_help(key),
            Mode::Command => self.handle_command_line(key),
            Mode::Search => self.handle_search(key),
            Mode::Palette => self.handle_palette(key),
        }
    }

    // ---- normal mode ---------------------------------------------------

    fn handle_normal(&mut self, key: KeyEvent) -> Outcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('q') => self.try_quit(),
                KeyCode::Char('k') => {
                    self.open_palette();
                    Outcome::Continue
                }
                KeyCode::Char('r') => {
                    self.redo();
                    Outcome::Continue
                }
                KeyCode::Char('n') => {
                    self.next_buffer();
                    Outcome::Continue
                }
                KeyCode::Char('p') => {
                    self.prev_buffer();
                    Outcome::Continue
                }
                _ => Outcome::Continue,
            };
        }

        match key.code {
            KeyCode::F(1) => {
                self.mode = Mode::Help;
                return Outcome::Continue;
            }
            KeyCode::Char(':') => {
                self.open_command_line();
                return Outcome::Continue;
            }
            KeyCode::Esc => {
                self.input = InputState::default();
                self.search = None;
                self.message = None;
                return Outcome::Continue;
            }
            KeyCode::Char(c) if c.is_ascii_digit() && (self.input.count.is_some() || c != '0') => {
                self.input.push_digit(c as usize - '0' as usize);
                return Outcome::Continue;
            }
            _ => {}
        }

        if let Some(op) = self.input.operator {
            self.apply_operator(op, key);
            return Outcome::Continue;
        }

        // Starting an operator keeps any count typed so far (`2d…` must remember the 2).
        let operator = match key.code {
            KeyCode::Char('d') => Some(Operator::Delete),
            KeyCode::Char('c') => Some(Operator::Change),
            KeyCode::Char('y') => Some(Operator::Yank),
            _ => None,
        };
        if let Some(op) = operator {
            self.input.operator = Some(op);
            return Outcome::Continue;
        }

        if self.input.g_prefix {
            self.input.g_prefix = false;
            if key.code == KeyCode::Char('g') {
                let count = self.input.take_count();
                self.jump_to_line(count);
            }
            return Outcome::Continue;
        }

        self.handle_normal_command(key);
        Outcome::Continue
    }

    fn handle_normal_command(&mut self, key: KeyEvent) {
        let had_count = self.input.count.is_some();
        let count = self.input.take_count();
        let cursor = self.document().cursor();
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => self.doc_mut().move_left(count),
            KeyCode::Char('j') | KeyCode::Down => self.doc_mut().move_down(count),
            KeyCode::Char('k') | KeyCode::Up => self.doc_mut().move_up(count),
            KeyCode::Char('l') | KeyCode::Right => self.doc_mut().move_right(count),
            KeyCode::Char('w') => self.doc_mut().move_word_forward(count),
            KeyCode::Char('b') => self.doc_mut().move_word_backward(count),
            KeyCode::Home | KeyCode::Char('0') => {
                self.doc_mut().set_cursor(Pos::new(cursor.line, 0));
            }
            KeyCode::End | KeyCode::Char('$') => self.doc_mut().move_line_end(),
            KeyCode::Char('G') => {
                let line = if had_count {
                    count
                } else {
                    self.document().line_count()
                };
                self.jump_to_line(line);
            }
            KeyCode::Char('g') => self.input.g_prefix = true,
            KeyCode::Char('i') => self.enter_insert(),
            KeyCode::Char('a') => {
                self.move_cursor_right(1);
                self.enter_insert();
            }
            KeyCode::Char('I') => {
                self.doc_mut().set_cursor(Pos::new(cursor.line, 0));
                self.enter_insert();
            }
            KeyCode::Char('A') => {
                let end = self.document().line_len(cursor.line);
                self.doc_mut().set_cursor(Pos::new(cursor.line, end));
                self.enter_insert();
            }
            KeyCode::Char('o') => {
                self.checkpoint();
                self.insert_started = true;
                let end = self.document().line_len(cursor.line);
                self.doc_mut().set_cursor(Pos::new(cursor.line, end));
                self.doc_mut().insert_newline();
                self.enter_insert();
            }
            KeyCode::Char('O') => {
                self.doc_mut().set_cursor(Pos::new(cursor.line, 0));
                self.doc_mut().insert_newline();
                self.doc_mut().set_cursor(Pos::new(cursor.line, 0));
                self.enter_insert();
            }
            KeyCode::Char('x') => self.delete_under_cursor(count),
            KeyCode::Char('p') => self.paste(false),
            KeyCode::Char('P') => self.paste(true),
            KeyCode::Char('u') => self.undo(),
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('n') => self.search_next(),
            KeyCode::Char('N') => self.search_previous(),
            _ => {}
        }
    }

    fn enter_insert(&mut self) {
        // The checkpoint is taken lazily, on the first key that actually changes the text, so an
        // insert session that types nothing leaves no useless undo step behind.
        self.insert_started = false;
        self.mode = Mode::Insert;
    }

    fn checkpoint(&mut self) {
        self.doc_mut().checkpoint();
    }

    fn undo(&mut self) {
        if self.doc_mut().undo() {
            self.message = None;
            self.doc_mut().clamp_normal();
        } else {
            self.message = Some("already at the oldest change".to_owned());
        }
    }

    fn redo(&mut self) {
        if self.doc_mut().redo() {
            self.message = None;
            self.doc_mut().clamp_normal();
        } else {
            self.message = Some("nothing to redo".to_owned());
        }
    }

    fn jump_to_line(&mut self, line: usize) {
        let target = line.saturating_sub(1).min(self.document().line_count().saturating_sub(1));
        self.doc_mut().set_cursor(Pos::new(target, 0));
        self.doc_mut().clamp_normal();
    }

    fn move_cursor_right(&mut self, count: usize) {
        let cursor = self.document().cursor();
        let col = (cursor.col + count).min(self.document().line_len(cursor.line));
        self.doc_mut().set_cursor(Pos::new(cursor.line, col));
    }

    fn delete_under_cursor(&mut self, count: usize) {
        let cursor = self.document().cursor();
        let at_last_char = cursor.col >= self.document().line_len(cursor.line)
            && cursor.line + 1 >= self.document().line_count();
        if at_last_char {
            return;
        }
        self.checkpoint();
        let mut removed = String::new();
        for _ in 0..count.max(1) {
            let cursor = self.document().cursor();
            let len = self.document().line_len(cursor.line);
            let end = if cursor.col < len {
                Pos::new(cursor.line, cursor.col + 1)
            } else if cursor.line + 1 < self.document().line_count() {
                Pos::new(cursor.line + 1, 0)
            } else {
                break;
            };
            removed.push_str(&self.doc_mut().remove(cursor, end));
            self.doc_mut().set_cursor(cursor);
        }
        if !removed.is_empty() {
            self.register = Some(Register {
                text: removed,
                linewise: false,
            });
        }
    }

    fn apply_operator(&mut self, op: Operator, key: KeyEvent) {
        self.input.operator = None;
        if key.code == KeyCode::Esc {
            return;
        }
        let count = self.input.take_count();

        let doubled = matches!(
            (op, key.code),
            (Operator::Delete, KeyCode::Char('d'))
                | (Operator::Change, KeyCode::Char('c'))
                | (Operator::Yank, KeyCode::Char('y'))
        );
        if doubled {
            self.apply_linewise(op, count);
            return;
        }

        let Some((start, end)) = self.operator_range(key.code, count) else {
            return;
        };
        if start == end {
            return;
        }

        let end = if op == Operator::Change && matches!(key.code, KeyCode::Char('w')) {
            // `cw` changes the word itself, leaving the space after it, like vim.
            let word = self.document().slice(start, end);
            let trimmed = word.trim_end_matches(char::is_whitespace);
            advance_pos(start, trimmed)
        } else {
            end
        };

        if op != Operator::Yank {
            self.checkpoint();
        }
        let text = self.document().slice(start, end);
        match op {
            Operator::Yank => {
                self.register = Some(Register {
                    text,
                    linewise: false,
                });
                self.doc_mut().set_cursor(start);
            }
            Operator::Delete => {
                self.doc_mut().remove(start, end);
                self.register = Some(Register {
                    text,
                    linewise: false,
                });
                self.doc_mut().set_cursor(start);
                self.doc_mut().clamp_normal();
            }
            Operator::Change => {
                self.doc_mut().remove(start, end);
                self.register = Some(Register {
                    text,
                    linewise: false,
                });
                self.doc_mut().set_cursor(start);
                self.enter_insert();
            }
        }
    }

    /// The half-open range an operator would touch if followed by `code`, found by running the
    /// motion on a copy of the buffer.
    fn operator_range(&self, code: KeyCode, count: usize) -> Option<(Pos, Pos)> {
        let start = self.document().cursor();
        let mut probe = self.document().clone();
        let inclusive = match code {
            KeyCode::Char('h') | KeyCode::Left => {
                probe.move_left(count);
                false
            }
            KeyCode::Char('l') | KeyCode::Right => {
                probe.move_right(count);
                false
            }
            KeyCode::Char('w') => {
                probe.move_word_forward(count);
                false
            }
            KeyCode::Char('b') => {
                probe.move_word_backward(count);
                false
            }
            KeyCode::Char('0') | KeyCode::Home => {
                probe.set_cursor(Pos::new(start.line, 0));
                false
            }
            KeyCode::Char('$') | KeyCode::End => {
                probe.move_line_end();
                true
            }
            _ => return None,
        };
        let target = probe.cursor();
        let (a, b) = ordered(start, target);
        if inclusive {
            Some((a, Pos::new(b.line, b.col + 1)))
        } else {
            Some((a, b))
        }
    }

    fn apply_linewise(&mut self, op: Operator, count: usize) {
        let line = self.document().cursor().line;
        let available = self.document().line_count().saturating_sub(line);
        let n = count.min(available.max(1));

        let lines: Vec<String> = (0..n).map(|i| self.document().line(line + i)).collect();
        let text = lines.join("\n");
        if op != Operator::Yank {
            self.checkpoint();
        }

        match op {
            Operator::Yank => {
                self.register = Some(Register {
                    text,
                    linewise: true,
                });
            }
            Operator::Delete => {
                for _ in 0..n {
                    self.doc_mut().delete_line(line);
                }
                self.register = Some(Register {
                    text,
                    linewise: true,
                });
            }
            Operator::Change => {
                for _ in 0..n {
                    self.doc_mut().delete_line(line);
                }
                self.doc_mut().insert_lines(line, &[String::new()]);
                self.doc_mut().set_cursor(Pos::new(line, 0));
                self.register = Some(Register {
                    text,
                    linewise: true,
                });
                self.enter_insert();
            }
        }
    }

    fn paste(&mut self, before: bool) {
        let Some(register) = self.register.clone() else {
            self.message = Some("nothing to paste yet".to_owned());
            return;
        };
        self.checkpoint();
        if register.linewise {
            let lines: Vec<String> = register.text.split('\n').map(str::to_owned).collect();
            let at = if before {
                self.document().cursor().line
            } else {
                self.document().cursor().line + 1
            };
            self.doc_mut().insert_lines(at, &lines);
            self.doc_mut().set_cursor(Pos::new(at, 0));
            self.doc_mut().clamp_normal();
        } else {
            let mut pos = self.document().cursor();
            if !before && pos.col < self.document().line_len(pos.line) {
                pos.col += 1;
            }
            self.doc_mut().insert_at(pos, &register.text);
            let mut line = pos.line;
            let mut col = pos.col;
            for ch in register.text.chars() {
                if ch == '\n' {
                    line += 1;
                    col = 0;
                } else {
                    col += 1;
                }
            }
            self.doc_mut().set_cursor(Pos::new(line, col));
            self.doc_mut().clamp_normal();
        }
    }


    // ---- search --------------------------------------------------------

    fn start_search(&mut self) {
        self.search = Some(Search {
            query: String::new(),
            origin: self.document().cursor(),
            origin_scroll: self.scroll,
        });
        self.mode = Mode::Search;
        self.message = None;
    }

    fn handle_search(&mut self, key: KeyEvent) -> Outcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return self.try_quit();
        }
        match key.code {
            KeyCode::Esc => self.cancel_search(),
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                if let Some(search) = self.search.as_mut() {
                    search.query.pop();
                }
                self.update_search_cursor();
            }
            KeyCode::Char(ch) => {
                if let Some(search) = self.search.as_mut() {
                    search.query.push(ch);
                }
                self.update_search_cursor();
            }
            _ => {}
        }
        Outcome::Continue
    }

    /// Give up the search: back to where `/` was pressed, highlight gone.
    fn cancel_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.doc_mut().set_cursor(search.origin);
            self.scroll = search.origin_scroll;
        }
        self.mode = Mode::Normal;
    }

    /// Jump to the first match from where the search started, wrapping once.
    fn update_search_cursor(&mut self) {
        let Some(search) = self.search.clone() else {
            return;
        };
        if search.query.is_empty() {
            self.doc_mut().set_cursor(search.origin);
            self.message = None;
            return;
        }
        let case_sensitive = is_case_sensitive(&search.query);
        let found = self
            .document()
            .find_forward(search.origin, &search.query, case_sensitive)
            .or_else(|| {
                self.document()
                    .find_forward(Pos::new(0, 0), &search.query, case_sensitive)
            });
        self.report_match(found, &search.query);
    }

    fn search_next(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            self.message = Some("no search yet — press / to search".to_owned());
            return;
        };
        if query.is_empty() {
            return;
        }
        let case_sensitive = is_case_sensitive(&query);
        let from = self.next_position(self.document().cursor());
        let found = self
            .document()
            .find_forward(from, &query, case_sensitive)
            .or_else(|| self.document().find_forward(Pos::new(0, 0), &query, case_sensitive));
        self.report_match(found, &query);
    }

    fn search_previous(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            self.message = Some("no search yet — press / to search".to_owned());
            return;
        };
        if query.is_empty() {
            return;
        }
        let case_sensitive = is_case_sensitive(&query);
        let cursor = self.document().cursor();
        let from = match self.previous_position(cursor) {
            Some(pos) => pos,
            None => return self.report_match(None, &query),
        };
        let found = self
            .document()
            .find_backward(from, &query, case_sensitive)
            .or_else(|| {
                let last = Pos::new(
                    self.document().line_count() - 1,
                    self.document().line_len(self.document().line_count() - 1),
                );
                self.document().find_backward(last, &query, case_sensitive)
            });
        self.report_match(found, &query);
    }

    fn report_match(&mut self, found: Option<Pos>, query: &str) {
        match found {
            Some(pos) => {
                self.doc_mut().set_cursor(pos);
                self.message = None;
            }
            None => self.message = Some(format!("no match: {query}")),
        }
    }

    /// The position one character after `pos`, for "search from after the cursor".
    fn next_position(&self, pos: Pos) -> Pos {
        if pos.col < self.document().line_len(pos.line) {
            Pos::new(pos.line, pos.col + 1)
        } else if pos.line + 1 < self.document().line_count() {
            Pos::new(pos.line + 1, 0)
        } else {
            pos
        }
    }

    /// The position one character before `pos`, for a backward search.
    fn previous_position(&self, pos: Pos) -> Option<Pos> {
        if pos.col > 0 {
            Some(Pos::new(pos.line, pos.col - 1))
        } else if pos.line > 0 {
            Some(Pos::new(pos.line - 1, self.document().line_len(pos.line - 1)))
        } else {
            None
        }
    }

    // ---- insert mode ---------------------------------------------------

    fn handle_insert(&mut self, key: KeyEvent) -> Outcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return self.try_quit();
        }
        let mutates = matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Delete
        );
        if mutates && !self.insert_started {
            self.checkpoint();
            self.insert_started = true;
        }
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                let cursor = self.document().cursor();
                if cursor.col > 0 {
                    let back = Pos::new(cursor.line, cursor.col - 1);
                    self.doc_mut().set_cursor(back);
                }
                self.doc_mut().clamp_normal();
            }
            KeyCode::Enter => self.doc_mut().insert_newline(),
            KeyCode::Tab => self.doc_mut().insert_char('\t'),
            KeyCode::Backspace => self.doc_mut().backspace(),
            KeyCode::Delete => self.doc_mut().delete_forward(),
            KeyCode::Left => self.doc_mut().move_left(1),
            KeyCode::Right => self.move_cursor_right(1),
            KeyCode::Up => self.doc_mut().move_up(1),
            KeyCode::Down => self.doc_mut().move_down(1),
            KeyCode::Char(c) => self.doc_mut().insert_char(c),
            _ => {}
        }
        Outcome::Continue
    }

    // ---- other modes ---------------------------------------------------

    fn handle_help(&mut self, key: KeyEvent) -> Outcome {
        if matches!(key.code, KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q')) {
            self.mode = Mode::Normal;
        }
        Outcome::Continue
    }

    fn handle_command_line(&mut self, key: KeyEvent) -> Outcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return self.try_quit();
        }
        match key.code {
            KeyCode::Esc => {
                self.command_line.clear();
                self.mode = Mode::Normal;
                Outcome::Continue
            }
            KeyCode::Enter => self.execute_command_line(),
            KeyCode::Backspace => {
                self.command_line.pop();
                if self.command_line.is_empty() {
                    self.mode = Mode::Normal;
                }
                Outcome::Continue
            }
            KeyCode::Char(c) => {
                self.command_line.push(c);
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn execute_command_line(&mut self) -> Outcome {
        let line = std::mem::take(&mut self.command_line);
        self.mode = Mode::Normal;
        let line = line.trim().to_owned();
        if line.is_empty() {
            return Outcome::Continue;
        }
        let (word, arg) = match line.split_once(char::is_whitespace) {
            Some((word, arg)) => (word, arg.trim()),
            None => (line.as_str(), ""),
        };
        match command::find(word) {
            Some(cmd) => self.run_command(cmd, arg),
            None => {
                self.message = Some(format!("not a command: {word}"));
                Outcome::Continue
            }
        }
    }

    fn run_command(&mut self, cmd: &Command, arg: &str) -> Outcome {
        match cmd.name {
            "write" => {
                if arg.is_empty() {
                    self.save();
                } else {
                    self.save_to(arg);
                }
                Outcome::Continue
            }
            "quit" => self.try_quit(),
            "quit-force" => Outcome::Quit,
            "write-quit" => {
                self.save();
                if self.buffers.iter().any(|doc| doc.is_dirty()) {
                    Outcome::Continue
                } else {
                    Outcome::Quit
                }
            }
            "search" => {
                self.start_search();
                Outcome::Continue
            }
            "search-next" => {
                self.search_next();
                Outcome::Continue
            }
            "search-previous" => {
                self.search_previous();
                Outcome::Continue
            }
            "undo" => {
                self.undo();
                Outcome::Continue
            }
            "redo" => {
                self.redo();
                Outcome::Continue
            }
            "next-buffer" => {
                self.next_buffer();
                Outcome::Continue
            }
            "prev-buffer" => {
                self.prev_buffer();
                Outcome::Continue
            }
            "buffer-delete" => {
                self.close_buffer();
                Outcome::Continue
            }
            "edit" => {
                if arg.is_empty() {
                    self.message = Some("edit needs a path: :e <path>".to_owned());
                } else {
                    self.open(arg);
                }
                Outcome::Continue
            }
            "help" => {
                self.mode = Mode::Help;
                Outcome::Continue
            }
            "palette" => {
                self.open_palette();
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn handle_palette(&mut self, key: KeyEvent) -> Outcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return self.try_quit();
        }
        match key.code {
            KeyCode::Esc => {
                self.palette_query.clear();
                self.mode = Mode::Normal;
                Outcome::Continue
            }
            KeyCode::Enter => {
                let selected = self.palette_matches().get(self.palette_selected).copied();
                self.palette_query.clear();
                self.mode = Mode::Normal;
                match selected {
                    Some(cmd) => self.run_command(cmd, ""),
                    None => {
                        self.message = Some("no matching command".to_owned());
                        Outcome::Continue
                    }
                }
            }
            KeyCode::Up => {
                self.palette_selected = self.palette_selected.saturating_sub(1);
                Outcome::Continue
            }
            KeyCode::Down => {
                let last = self.palette_matches().len().saturating_sub(1);
                self.palette_selected = (self.palette_selected + 1).min(last);
                Outcome::Continue
            }
            KeyCode::Backspace => {
                self.palette_query.pop();
                self.palette_selected = 0;
                Outcome::Continue
            }
            KeyCode::Char(c) => {
                self.palette_query.push(c);
                self.palette_selected = 0;
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn save_to(&mut self, path: &str) {
        let result = self.doc_mut().save_as(path);
        self.message = match result {
            Ok(()) => Some(format!("\"{path}\" written")),
            Err(err) => Some(format!("save failed: {err}")),
        };
    }

    /// Quitting is refused while any buffer has unsaved changes; nothing is thrown away silently.
    fn try_quit(&mut self) -> Outcome {
        match self.buffers.iter().find(|doc| doc.is_dirty()) {
            Some(dirty) => {
                self.message = Some(format!(
                    "unsaved changes in \"{}\" — :w or :q! to discard",
                    dirty.display_name()
                ));
                Outcome::Continue
            }
            None => Outcome::Quit,
        }
    }
}

/// Walk `text` forward from `start` to get the position just after it.
fn advance_pos(start: Pos, text: &str) -> Pos {
    let mut pos = start;
    for ch in text.chars() {
        if ch == '\n' {
            pos.line += 1;
            pos.col = 0;
        } else {
            pos.col += 1;
        }
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::{App, Outcome};
    use crate::document::Pos;
    use crate::mode::Mode;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::fs;
    use std::path::PathBuf;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn keys(app: &mut App, seq: &str) {
        for ch in seq.chars() {
            if ch == '<' {
                continue;
            }
            app.handle_key(key(KeyCode::Char(ch)));
        }
    }

    fn esc(app: &mut App) {
        app.handle_key(key(KeyCode::Esc));
    }

    /// An app holding `text`, cursor at the start, plus the file it was opened from. Each call gets
    /// its own file so tests can run in parallel.
    fn app_with_path(text: &str) -> (App, PathBuf) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let mut app = App::new();
        let dir = std::env::temp_dir().join(format!(
            "nyavim-app-keys-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("buffer.txt");
        fs::write(&path, text).unwrap();
        app.open(&path);
        (app, path)
    }

    fn app_with(text: &str) -> App {
        app_with_path(text).0
    }

    fn enter(app: &mut App) {
        app.handle_key(key(KeyCode::Enter));
    }

    /// Type a `:` command line and run it.
    fn run_cmd(app: &mut App, line: &str) {
        let _ = run_cmd_outcome(app, line);
    }

    fn run_cmd_outcome(app: &mut App, line: &str) -> Outcome {
        app.handle_key(key(KeyCode::Char(':')));
        for ch in line.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter))
    }

    fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nyavim-app-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn starts_in_normal_mode() {
        assert_eq!(App::new().mode(), Mode::Normal);
    }

    #[test]
    fn f1_opens_help_and_esc_returns() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::F(1)));
        assert_eq!(app.mode(), Mode::Help);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode(), Mode::Normal);
    }

    #[test]
    fn command_line_and_palette_are_enterable() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char(':')));
        assert_eq!(app.mode(), Mode::Command);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode(), Mode::Normal);

        app.handle_key(ctrl('k'));
        assert_eq!(app.mode(), Mode::Palette);
    }

    #[test]
    fn ctrl_q_quits_a_clean_buffer() {
        let mut app = App::new();
        assert_eq!(app.handle_key(ctrl('q')), Outcome::Quit);
    }

    #[test]
    fn ctrl_q_is_refused_while_dirty() {
        let mut app = app_with("hello");
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('x')));
        esc(&mut app);
        assert_eq!(app.handle_key(ctrl('q')), Outcome::Continue);
        assert!(app.message().unwrap().contains("unsaved"));
    }

    #[test]
    fn opening_a_missing_file_starts_an_empty_buffer_with_that_name() {
        let mut app = App::new();
        app.open(temp_file("placeholder.txt", b"").with_file_name("brand-new.txt"));
        assert_eq!(app.document().display_name(), "brand-new.txt");
        assert!(!app.document().is_dirty());
        assert!(app.message().is_none());
    }

    #[test]
    fn opening_text_fills_the_buffer() {
        let mut app = App::new();
        app.open(temp_file("hello.txt", b"hello\nworld\n"));
        assert_eq!(app.document().line_count(), 2);
        assert_eq!(app.document().line(1), "world");
    }

    #[test]
    fn unreadable_file_leaves_a_usable_editor_with_a_message() {
        let mut app = App::new();
        app.open(temp_file("blob.bin", &[0xff, 0x00, 0xfe]));
        assert!(app.message().unwrap().contains("not UTF-8"));
        assert_eq!(app.document().display_name(), "[No Name]");
    }

    #[test]
    fn hjkl_moves_the_cursor() {
        let mut app = app_with("abc\ndef\n");
        keys(&mut app, "ll");
        assert_eq!(app.document().cursor(), Pos::new(0, 2));
        keys(&mut app, "j");
        assert_eq!(app.document().cursor(), Pos::new(1, 2));
        keys(&mut app, "h");
        assert_eq!(app.document().cursor(), Pos::new(1, 1));
        keys(&mut app, "k");
        assert_eq!(app.document().cursor(), Pos::new(0, 1));
    }

    #[test]
    fn counts_repeat_motions() {
        let mut app = app_with("abcdef\n");
        keys(&mut app, "4l");
        assert_eq!(app.document().cursor(), Pos::new(0, 4));
        keys(&mut app, "2h");
        assert_eq!(app.document().cursor(), Pos::new(0, 2));
    }

    #[test]
    fn insert_typing_and_escape_writes_text() {
        let mut app = app_with("world\n");
        keys(&mut app, "ihello ");
        esc(&mut app);
        assert_eq!(app.document().line(0), "hello world");
        assert!(app.document().is_dirty());
        assert_eq!(app.mode(), Mode::Normal);
    }

    #[test]
    fn append_and_open_line() {
        let mut app = app_with("ab\n");
        keys(&mut app, "A!");
        esc(&mut app);
        assert_eq!(app.document().line(0), "ab!");

        keys(&mut app, "o");
        keys(&mut app, "next");
        esc(&mut app);
        assert_eq!(app.document().line(1), "next");
    }

    #[test]
    fn dd_removes_the_line_and_p_pastes_it_below() {
        let mut app = app_with("one\ntwo\nthree\n");
        keys(&mut app, "dd");
        assert_eq!(app.document().text(), "two\nthree\n");
        keys(&mut app, "p");
        // vim pastes below the cursor line, so the deleted line lands under "two"
        assert_eq!(app.document().text(), "two\none\nthree\n");
    }

    #[test]
    fn count_dd_removes_several_lines() {
        let mut app = app_with("a\nb\nc\nd\n");
        keys(&mut app, "2dd");
        assert_eq!(app.document().line(0), "c");
        keys(&mut app, "p");
        assert_eq!(app.document().text(), "c\na\nb\nd\n");
    }

    #[test]
    fn yy_then_p_duplicates_a_line() {
        let mut app = app_with("keep\nleave\n");
        keys(&mut app, "yyp");
        assert_eq!(app.document().text(), "keep\nkeep\nleave\n");
    }

    #[test]
    fn dw_and_dollar_respect_word_boundaries() {
        let mut app = app_with("alpha beta gamma\n");
        keys(&mut app, "dw");
        assert_eq!(app.document().line(0), "beta gamma");

        keys(&mut app, "d$");
        assert_eq!(app.document().line(0), "");
    }

    #[test]
    fn x_deletes_under_the_cursor_and_p_restores_it() {
        let mut app = app_with("abc\n");
        keys(&mut app, "x");
        assert_eq!(app.document().line(0), "bc");
        keys(&mut app, "p");
        assert_eq!(app.document().line(0), "bac");
    }

    #[test]
    fn cw_changes_a_word() {
        let mut app = app_with("alpha beta\n");
        keys(&mut app, "cw");
        assert_eq!(app.mode(), Mode::Insert);
        keys(&mut app, "one");
        esc(&mut app);
        assert_eq!(app.document().line(0), "one beta");
    }

    #[test]
    fn gg_and_capital_g_jump_to_the_ends() {
        let mut app = app_with("1\n2\n3\n4\n");
        keys(&mut app, "G");
        assert_eq!(app.document().cursor(), Pos::new(3, 0));
        keys(&mut app, "gg");
        assert_eq!(app.document().cursor(), Pos::new(0, 0));
        keys(&mut app, "3G");
        assert_eq!(app.document().cursor(), Pos::new(2, 0));
    }

    #[test]
    fn escape_clears_a_half_typed_command() {
        let mut app = app_with("abc\n");
        keys(&mut app, "2d");
        assert_eq!(app.pending_keys(), "2d");
        esc(&mut app);
        assert_eq!(app.pending_keys(), "");
        assert_eq!(app.document().line(0), "abc");
    }

    #[test]
    fn follow_cursor_scrolls_only_when_needed() {
        let text = (0..100).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let mut app = app_with(&text);
        app.follow_cursor(10);
        assert_eq!(app.scroll(), 0);
        keys(&mut app, "30j");
        app.follow_cursor(10);
        assert!(app.scroll() > 0);
        assert!(app.document().cursor().line < app.scroll() + 10);
        assert!(app.document().cursor().line >= app.scroll());
    }

    #[test]
    fn command_line_w_saves_the_buffer() {
        let (mut app, path) = app_with_path("hello\n");
        keys(&mut app, "iX");
        esc(&mut app);
        run_cmd(&mut app, "w");
        assert!(!app.document().is_dirty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "Xhello\n");
        assert!(app.message().unwrap().contains("written"));
    }

    #[test]
    fn command_line_q_is_refused_while_dirty_and_q_bang_quits() {
        let mut app = app_with("hello\n");
        keys(&mut app, "iX");
        esc(&mut app);
        assert_eq!(run_cmd_outcome(&mut app, "q"), Outcome::Continue);
        assert!(app.message().unwrap().contains("unsaved"));
        assert_eq!(run_cmd_outcome(&mut app, "q!"), Outcome::Quit);
    }

    #[test]
    fn command_line_edit_opens_another_file() {
        let (mut app, path) = app_with_path("first\n");
        let other = path.with_file_name("other.txt");
        fs::write(&other, "second\n").unwrap();
        run_cmd(&mut app, &format!("e {}", other.display()));
        assert_eq!(app.document().line(0), "second");
        assert_eq!(app.document().display_name(), "other.txt");
    }

    #[test]
    fn unknown_command_reports_instead_of_crashing() {
        let mut app = app_with("x\n");
        run_cmd(&mut app, "frobnicate");
        assert!(app.message().unwrap().contains("not a command"));
        assert_eq!(app.mode(), Mode::Normal);
    }

    #[test]
    fn palette_searches_by_what_a_command_does_and_runs_it() {
        let (mut app, path) = app_with_path("hello\n");
        keys(&mut app, "iX");
        esc(&mut app);

        app.handle_key(ctrl('k'));
        assert_eq!(app.mode(), Mode::Palette);
        for ch in "save".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(app.palette_matches()[0].name, "write");

        enter(&mut app);
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(fs::read_to_string(&path).unwrap(), "Xhello\n");
    }

    #[test]
    fn palette_escape_leaves_the_buffer_alone() {
        let mut app = app_with("abc\n");
        app.handle_key(ctrl('k'));
        app.handle_key(key(KeyCode::Char('q')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.document().line(0), "abc");
    }

    #[test]
    fn palette_selection_wraps_through_the_list() {
        let mut app = app_with("abc\n");
        app.handle_key(ctrl('k'));
        let count = app.palette_matches().len();
        for _ in 0..count + 3 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(app.palette_selected(), count - 1);
        for _ in 0..count + 3 {
            app.handle_key(key(KeyCode::Up));
        }
        assert_eq!(app.palette_selected(), 0);
    }

    #[test]
    fn palette_can_open_the_help_page() {
        let mut app = app_with("abc\n");
        app.handle_key(ctrl('k'));
        for ch in "help".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        enter(&mut app);
        assert_eq!(app.mode(), Mode::Help);
    }

    #[test]
    fn opening_a_second_file_keeps_the_first_one_open() {
        let (mut app, path) = app_with_path("first\n");
        let other = path.with_file_name("second.txt");
        fs::write(&other, "second\n").unwrap();

        run_cmd(&mut app, &format!("e {}", other.display()));
        assert_eq!(app.buffers().len(), 2);
        assert_eq!(app.active(), 1);
        assert_eq!(app.document().line(0), "second");

        app.handle_key(ctrl('n'));
        assert_eq!(app.active(), 0);
        assert_eq!(app.document().line(0), "first");
        app.handle_key(ctrl('p'));
        assert_eq!(app.active(), 1);
    }

    #[test]
    fn reopening_an_open_file_switches_instead_of_duplicating() {
        let (mut app, path) = app_with_path("first\n");
        let other = path.with_file_name("second.txt");
        fs::write(&other, "second\n").unwrap();

        run_cmd(&mut app, &format!("e {}", other.display()));
        run_cmd(&mut app, &format!("e {}", path.display()));
        assert_eq!(app.buffers().len(), 2);
        assert_eq!(app.active(), 0);
        assert_eq!(app.document().display_name(), "buffer.txt");
    }

    #[test]
    fn closing_a_dirty_buffer_is_refused() {
        let (mut app, path) = app_with_path("first\n");
        let other = path.with_file_name("second.txt");
        fs::write(&other, "second\n").unwrap();
        run_cmd(&mut app, &format!("e {}", other.display()));

        keys(&mut app, "iX");
        esc(&mut app);
        run_cmd(&mut app, "bd");
        assert_eq!(app.buffers().len(), 2);
        assert!(app.message().unwrap().contains("unsaved"));

        run_cmd(&mut app, "w");
        run_cmd(&mut app, "bd");
        assert_eq!(app.buffers().len(), 1);
        assert_eq!(app.document().display_name(), "buffer.txt");
    }

    #[test]
    fn quit_is_refused_while_a_background_buffer_is_dirty() {
        let (mut app, path) = app_with_path("first\n");
        let other = path.with_file_name("second.txt");
        fs::write(&other, "second\n").unwrap();

        keys(&mut app, "iX");
        esc(&mut app);
        run_cmd(&mut app, &format!("e {}", other.display()));
        assert_eq!(run_cmd_outcome(&mut app, "q"), Outcome::Continue);
        assert!(app.message().unwrap().contains("unsaved"));
    }

    #[test]
    fn the_startup_scratch_buffer_is_replaced_by_the_first_file() {
        let mut app = App::new();
        let dir = std::env::temp_dir().join(format!("nyavim-scratch-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("one.txt");
        fs::write(&path, "one\n").unwrap();

        app.open(&path);
        assert_eq!(app.buffers().len(), 1);
        assert_eq!(app.document().display_name(), "one.txt");
    }

    #[test]
    fn u_undoes_a_whole_insert_session() {
        let mut app = app_with("start\n");
        keys(&mut app, "ihello ");
        esc(&mut app);
        assert_eq!(app.document().line(0), "hello start");
        keys(&mut app, "u");
        assert_eq!(app.document().line(0), "start");
        assert!(!app.document().is_dirty());
    }

    #[test]
    fn ctrl_r_redoes_what_u_undid() {
        let mut app = app_with("start\n");
        keys(&mut app, "ihello ");
        esc(&mut app);
        keys(&mut app, "u");
        app.handle_key(ctrl('r'));
        assert_eq!(app.document().line(0), "hello start");
    }

    #[test]
    fn undo_brings_a_deleted_line_back() {
        let mut app = app_with("one\ntwo\n");
        keys(&mut app, "dd");
        assert_eq!(app.document().text(), "two\n");
        keys(&mut app, "u");
        assert_eq!(app.document().text(), "one\ntwo\n");
    }

    #[test]
    fn an_empty_insert_session_leaves_no_undo_step() {
        let mut app = app_with("keep\n");
        keys(&mut app, "i");
        esc(&mut app);
        keys(&mut app, "u");
        assert!(app.message().unwrap().contains("oldest change"));
        assert_eq!(app.document().line(0), "keep");
    }

    #[test]
    fn a_whole_session_opens_edits_saves_and_quits() {
        let dir = std::env::temp_dir().join(format!("nyavim-e2e-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.txt");
        fs::write(&path, "first line\n").unwrap();

        let mut app = App::new();
        app.open(&path);
        keys(&mut app, "j"); // nowhere to go on a one-line file
        keys(&mut app, "A: done");
        esc(&mut app);
        run_cmd(&mut app, "w");
        assert_eq!(
            run_cmd_outcome(&mut app, "q"),
            Outcome::Quit,
            "a saved buffer may quit"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "first line: done\n");
    }

    fn search(app: &mut App, query: &str) {
        app.handle_key(key(KeyCode::Char('/')));
        for ch in query.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn incremental_search_jumps_to_the_first_match() {
        let mut app = app_with("one\ntwo beta\nbeta three\n");
        search(&mut app, "beta");
        assert_eq!(app.mode(), Mode::Search);
        assert_eq!(app.document().cursor(), Pos::new(1, 4));
        assert_eq!(app.search_progress(), Some((1, 2, false)));

        enter(&mut app);
        assert_eq!(app.mode(), Mode::Normal);
        assert!(app.search_highlight().is_some());

        keys(&mut app, "n");
        assert_eq!(app.document().cursor(), Pos::new(2, 0));
        assert_eq!(app.search_progress().unwrap().0, 2);
        keys(&mut app, "N");
        assert_eq!(app.document().cursor(), Pos::new(1, 4));
    }

    #[test]
    fn escape_cancels_search_and_restores_the_cursor() {
        let mut app = app_with("one\nbeta\n");
        search(&mut app, "beta");
        assert_eq!(app.document().cursor(), Pos::new(1, 0));
        esc(&mut app);
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.document().cursor(), Pos::new(0, 0));
        assert!(app.search_highlight().is_none());
        assert!(app.search_progress().is_none());
    }

    #[test]
    fn search_wraps_and_reports_when_nothing_matches() {
        let mut app = app_with("beta\nbeta\n");
        search(&mut app, "beta");
        enter(&mut app);
        keys(&mut app, "n");
        assert_eq!(app.document().cursor(), Pos::new(1, 0));
        keys(&mut app, "n");
        assert_eq!(app.document().cursor(), Pos::new(0, 0), "wraps around");

        esc(&mut app);
        search(&mut app, "zzz");
        assert!(app.message().unwrap().contains("no match"));
    }

    #[test]
    fn search_next_without_a_search_explains_itself() {
        let mut app = app_with("abc\n");
        keys(&mut app, "n");
        assert!(app.message().unwrap().contains("press /"));
    }

    #[test]
    fn editing_with_enter_in_insert_mode_does_not_start_a_search() {
        let mut app = app_with("abc\n");
        keys(&mut app, "i");
        enter(&mut app);
        assert_eq!(app.mode(), Mode::Insert);
        assert_eq!(app.document().line_count(), 2);
    }
}
