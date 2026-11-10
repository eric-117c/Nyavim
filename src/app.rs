//! Application state and key routing. Rendering lives in [`crate::ui`]; text and files live in
//! [`crate::document`]. Nothing here touches the terminal, so the whole thing is testable without a
//! tty.

use std::path::Path;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::clipboard;
use crate::command::{self, Command, Substitute};
use crate::config::Config;
use crate::document::{is_case_sensitive, ordered, Document, LoadError, Pos};
use crate::finder;
use crate::git::{self, GitInfo};
use crate::keymap::{self, Action, Bindings, InsertKind, Motion, Operator};
use crate::layout::{self, Layout, SplitDir};
use crate::mode::Mode;
use crate::swap;
use crate::syntax::{self, Language, State};
use crate::terminal::Session;

/// What the main loop should do after a key has been handled.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
}

/// The search in flight or the last one, kept so `n`/`N` and the highlight keep working after the
/// prompt closes.
#[derive(Debug, Clone)]
struct Search {
    query: String,
    origin: Pos,
    origin_scroll: usize,
    /// `/` searches forward, `?` searches backward; `n` keeps going that way.
    forward: bool,
}

/// Where one view of a buffer is looking. Cursors and scrolling belong to the view, not the text:
/// two windows on the same file each keep their own position.
#[derive(Debug, Clone, Copy, Default)]
pub struct Window {
    /// Index into [`App::buffers`].
    pub buffer: usize,
    pub cursor: Pos,
    pub scroll: usize,
    /// Highlight state at the first line on screen, kept up to date by [`App::follow_cursor`].
    pub highlight: State,
    /// Where the incremental state walk resumes from: the line and its state.
    anchor_line: usize,
    anchor_state: State,
}

/// The last change, kept so `.` can replay it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Repeat {
    Operator(Operator, Motion),
    DeleteChar,
    Paste(bool),
    Insert(InsertKind, String),
    /// A visual selection: how many characters it covered, so `.` can take the same amount again.
    Visual(Operator, usize),
}

/// A `c` substitute walking through its matches, collecting decisions before touching the buffer.
#[derive(Debug)]
struct Replacer {
    spec: Substitute,
    matches: Vec<Pos>,
    index: usize,
    approved: Vec<Pos>,
}

/// How many matches the status line bothers to count before it says "and more", and how many lines
/// it scans looking for them. Both keep a keystroke responsive on a very large file.
const MATCH_COUNT_CAP: usize = 5000;
const MATCH_SCAN_LINES: usize = 20_000;

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
    /// `i` / `a` after an operator: `Some(true)` for inner, `Some(false)` for around.
    text_object: Option<bool>,
    /// Register chosen with `"`, or `None` for the unnamed one.
    register: Option<char>,
    /// A `"` was typed and the register name is the next key.
    awaiting_register: bool,
}

impl InputState {
    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1).max(1)
    }

    fn push_digit(&mut self, digit: usize) {
        let n = self
            .count
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add(digit);
        self.count = Some(n);
    }
}

pub struct App {
    mode: Mode,
    /// Message shown next to the mode indicator, e.g. a refused save.
    message: Option<String>,
    buffers: Vec<Document>,
    /// One entry per pane; the layout tree decides where each is drawn.
    /// Git view of each buffer, index-parallel with `buffers`.
    git: Vec<GitInfo>,
    windows: Vec<Window>,
    layout: Layout,
    focus: usize,
    /// Terminal area, updated by the main loop so focus movement can reason about geometry.
    area: Rect,
    input: InputState,
    register: Option<Register>,
    /// Named registers (`"ay`, `"ap`).
    named_registers: std::collections::HashMap<char, Register>,
    /// Last change, for `.`.
    last_change: Option<Repeat>,
    /// How the current insert session was entered, so `.` can reopen it the same way.
    insert_kind: InsertKind,
    /// Text typed during the current insert session, for `.` on inserts.
    insert_typed: String,
    /// Where a visual selection started.
    visual_anchor: Option<Pos>,
    /// Whether the current insert session has taken its undo checkpoint yet.
    insert_started: bool,
    search: Option<Search>,
    replacer: Option<Replacer>,
    /// Swap content waiting for a recover/discard answer.
    swap_prompt: Option<String>,
    /// When the buffer was last mirrored to its swap file.
    last_swap_write: Option<std::time::Instant>,
    /// User configuration; also the live source of tab width, scroll margin and swap interval.
    config: Config,
    /// Live binding table: defaults plus whatever `[keys]` asked for.
    keymap: Bindings,
    /// The shell session while the terminal owns the screen.
    terminal: Option<Session>,
    /// Commit message being typed after `:commit`.
    commit_message: String,
    /// `Ctrl+P` state: the project root being listed, its files, the query and the selection.
    finder_root: Option<std::path::PathBuf>,
    finder_files: Vec<String>,
    finder_query: String,
    finder_selected: usize,
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
            git: vec![GitInfo::default()],
            windows: vec![Window::default()],
            layout: Layout::leaf(0),
            focus: 0,
            area: Rect::new(0, 0, 80, 24),
            input: InputState::default(),
            register: None,
            named_registers: std::collections::HashMap::new(),
            visual_anchor: None,
            last_change: None,
            insert_typed: String::new(),
            insert_kind: InsertKind::Here,
            insert_started: false,
            search: None,
            replacer: None,
            config: Config::default(),
            keymap: Bindings::default(),
            swap_prompt: None,
            last_swap_write: None,
            terminal: None,
            commit_message: String::new(),
            finder_root: None,
            finder_files: Vec::new(),
            finder_query: String::new(),
            finder_selected: 0,
            command_line: String::new(),
            palette_query: String::new(),
            palette_selected: 0,
        }
    }

    pub fn set_config(&mut self, mut config: Config) {
        if let Some(warning) = config.warnings.first().cloned() {
            self.message = Some(format!("config: {warning}"));
            config.warnings.clear();
        }
        self.keymap = Bindings::default();
        self.keymap.apply_overrides(&config.keys);
        if let Some(warning) = self.keymap.warnings().first().cloned() {
            self.message = Some(format!("config: {warning}"));
        }
        self.config = config;
    }

    /// The live binding table, for the help page.
    pub fn bindings(&self) -> Vec<(String, Action)> {
        self.keymap.entries()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn document(&self) -> &Document {
        &self.buffers[self.windows[self.focus].buffer]
    }

    fn doc_mut(&mut self) -> &mut Document {
        let idx = self.windows[self.focus].buffer;
        &mut self.buffers[idx]
    }

    /// Which tab the given screen column falls on, mirroring the tab bar's layout.
    fn tab_at(&self, x: u16) -> Option<usize> {
        let mut start = 0u16;
        for (index, doc) in self.buffers.iter().enumerate() {
            let width = crate::ui::tab_label(doc).chars().count() as u16;
            if x >= start && x < start + width {
                return Some(index);
            }
            start += width + 1; // the separator cell
        }
        None
    }

    /// Left click: switch tabs, or focus the window that was clicked and put the cursor there.
    pub fn mouse_click(&mut self, x: u16, y: u16) {
        if y == 0 {
            if let Some(index) = self.tab_at(x) {
                self.refresh_git();
                self.switch_to(index);
            }
            return;
        }
        let Some((id, rect)) = self.window_rects().into_iter().find(|(_, rect)| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
        }) else {
            return;
        };
        self.focus = id;
        let Some(window) = self.windows.get(id).copied() else {
            return;
        };
        let text_width = {
            let gutter = crate::ui::gutter_width(self.document(), self.git_info().in_repo());
            (rect.width as usize).saturating_sub(gutter)
        };
        let rows = crate::ui::visible_rows(
            self.document(),
            window.scroll,
            rect.height as usize,
            text_width,
            self.config.tab_width,
            self.config.wrap,
        );
        let Some((line, start)) = rows.get((y - rect.y) as usize).copied() else {
            return;
        };
        let column = self.column_at_screen_x(line, rect.x, x).max(start);
        self.set_cursor(Pos::new(line, column));
        self.clamp_cursor();
    }

    /// Wheel: scroll the window under the pointer without moving the cursor.
    pub fn mouse_scroll(&mut self, x: u16, y: u16, delta: isize) {
        let Some((id, _)) = self
            .window_rects()
            .into_iter()
            .find(|(_, rect)| x >= rect.x && x < rect.x + rect.width && y >= rect.y)
        else {
            return;
        };
        let line_count = self.buffers[self.windows[id].buffer].line_count();
        let max_top = line_count.saturating_sub(1);
        let current = self.windows[id].scroll as isize;
        self.windows[id].scroll = (current + delta).clamp(0, max_top as isize) as usize;
        self.sync_highlight();
    }

    /// Character column under a screen x, accounting for the gutter and for wide characters/tabs.
    fn column_at_screen_x(&self, line: usize, rect_x: u16, x: u16) -> usize {
        let doc = self.document();
        let number_width = crate::ui::digits(doc.line_count()).max(3);
        let marks = if self.git_info().in_repo() { 1 } else { 0 };
        let gutter = marks + number_width + 1;
        let target = (x.saturating_sub(rect_x) as usize).saturating_sub(gutter);

        let text = doc.line(line);
        let mut column = 0;
        let mut width = 0;
        for ch in text.chars() {
            let cell = if ch == '\t' {
                self.config.tab_width
            } else {
                unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
            };
            if width + cell > target {
                break;
            }
            width += cell;
            column += 1;
        }
        column
    }

    /// Cursor of the focused view.
    pub fn cursor(&self) -> Pos {
        self.windows[self.focus].cursor
    }

    /// Move the focused cursor, clamped into the buffer.
    fn set_cursor(&mut self, pos: Pos) {
        let clamped = self.document().clamp_pos(pos);
        self.windows[self.focus].cursor = clamped;
    }

    /// Keep the cursor on a character in normal mode.
    fn clamp_cursor(&mut self) {
        let clamped = self.document().clamp_normal_pos(self.cursor());
        self.windows[self.focus].cursor = clamped;
    }

    fn motion(&mut self, f: impl FnOnce(&Document, Pos) -> Pos) {
        let next = f(self.document(), self.cursor());
        self.set_cursor(next);
    }

    fn edit(&mut self, f: impl FnOnce(&mut Document, &mut Pos)) {
        if self.document().is_read_only() {
            self.message = Some("read-only: file is larger than 50 MB".to_owned());
            return;
        }
        let idx = self.windows[self.focus].buffer;
        let mut cursor = self.windows[self.focus].cursor;
        f(&mut self.buffers[idx], &mut cursor);
        let clamped = self.buffers[idx].clamp_pos(cursor);
        self.windows[self.focus].cursor = clamped;
    }

    // ---- windows -------------------------------------------------------

    pub fn set_area(&mut self, width: u16, height: u16) {
        self.area = Rect::new(0, 0, width, height);
    }

    /// Where each window is drawn, in tab order of the layout tree.
    pub fn window_rects(&self) -> Vec<(usize, Rect)> {
        let mut rects = Vec::new();
        self.layout.rects(self.body_area(), &mut rects);
        rects
    }

    /// Separator cells implied by the current splits.
    pub fn separators(&self) -> Vec<(SplitDir, Rect)> {
        let mut out = Vec::new();
        self.layout.separators(self.body_area(), &mut out);
        out
    }

    /// Area left for buffer text once the tab bar, status line and hint line are gone.
    fn body_area(&self) -> Rect {
        Rect {
            x: 0,
            y: 1,
            width: self.area.width,
            height: self.area.height.saturating_sub(3),
        }
    }

    pub fn window_snapshot(&self, window: usize) -> Option<&Window> {
        self.windows.get(window)
    }

    /// The buffer at `index`, for rendering a specific window.
    pub fn buffer(&self, index: usize) -> Option<&Document> {
        self.buffers.get(index)
    }

    pub fn focused_window(&self) -> usize {
        self.focus
    }

    /// `:resize +5` / `:resize -5`: move the split around the focused window.
    pub fn resize_window(&mut self, delta: i16) {
        if self.windows.len() <= 1 {
            self.message = Some("only one window".to_owned());
            return;
        }
        if self.layout.adjust_ratio(self.focus, delta) {
            self.message = Some(format!("resized by {delta:+}%"));
        }
    }

    /// Split the focused window, showing the same buffer at the same place in both halves.
    pub fn split_window(&mut self, dir: SplitDir) {
        let current = self.windows[self.focus];
        let new_window = self.windows.len();
        self.windows.push(Window {
            scroll: 0,
            ..current
        });
        if !self.layout.split_leaf(self.focus, dir, new_window) {
            self.windows.pop();
            return;
        }
        self.focus = new_window;
    }

    /// Close the focused window. Refused when it is the last one.
    pub fn close_window(&mut self) {
        if self.windows.len() <= 1 {
            self.message = Some("this is the last window — :q to quit".to_owned());
            return;
        }
        let closing = self.focus;
        if !self.layout.remove_leaf(closing) {
            return;
        }
        self.windows.remove(closing);
        self.layout
            .remap(&|id| if id > closing { id - 1 } else { id });
        self.focus = self.layout.leaves().first().copied().unwrap_or(0);
        let index = self.windows[self.focus].buffer;
        let _ = index;
    }

    /// Close every window except the focused one.
    pub fn only_window(&mut self) {
        let keep = self.windows[self.focus];
        self.windows = vec![keep];
        self.layout = Layout::leaf(0);
        self.focus = 0;
    }

    pub fn focus_next_window(&mut self) {
        let leaves = self.layout.leaves();
        if leaves.len() <= 1 {
            return;
        }
        let position = leaves.iter().position(|id| *id == self.focus).unwrap_or(0);
        self.focus = leaves[(position + 1) % leaves.len()];
    }

    /// Move focus to the window in `direction` (`h`, `j`, `k`, `l`).
    pub fn focus_direction(&mut self, direction: char) {
        let rects = self.window_rects();
        if let Some(target) = layout::neighbor(&rects, self.focus, direction) {
            self.focus = target;
        } else {
            self.message = Some(format!("no window to the {direction}"));
        }
    }

    /// Every open buffer, in tab order.
    pub fn buffers(&self) -> &[Document] {
        &self.buffers
    }

    /// Index of the buffer shown by the focused window.
    pub fn active(&self) -> usize {
        self.windows[self.focus].buffer
    }

    fn switch_to(&mut self, idx: usize) {
        if idx < self.buffers.len() {
            self.refresh_git();
            self.windows[self.focus].buffer = idx;
            self.windows[self.focus].cursor = Pos::default();
            self.windows[self.focus].scroll = 0;
            self.input = InputState::default();
        }
    }

    /// Show the next buffer, wrapping around.
    pub fn next_buffer(&mut self) {
        if self.buffers.len() > 1 {
            let next = (self.active() + 1) % self.buffers.len();
            self.switch_to(next);
        } else {
            self.message = Some("only one buffer open".to_owned());
        }
    }

    /// Show the previous buffer, wrapping around.
    pub fn prev_buffer(&mut self) {
        if self.buffers.len() > 1 {
            let prev = (self.active() + self.buffers.len() - 1) % self.buffers.len();
            self.switch_to(prev);
        } else {
            self.message = Some("only one buffer open".to_owned());
        }
    }

    /// Close the buffer shown by the focused window, refusing when it has unsaved changes. Other
    /// windows showing it fall back to the neighbouring buffer rather than closing.
    pub fn close_buffer(&mut self) {
        if self.document().is_dirty() {
            self.message = Some(format!(
                "\"{}\" has unsaved changes — :w first",
                self.document().display_name()
            ));
            return;
        }
        let closing = self.active();
        if self.buffers.len() == 1 {
            self.git[0] = GitInfo::default();
            self.buffers[0] = Document::empty();
            for window in &mut self.windows {
                window.buffer = 0;
                window.cursor = Pos::default();
                window.scroll = 0;
            }
        } else {
            self.buffers.remove(closing);
            if closing < self.git.len() {
                self.git.remove(closing);
            }
            let last = self.buffers.len() - 1;
            let fallback = closing.min(last);
            for window in &mut self.windows {
                if window.buffer == closing {
                    window.buffer = fallback;
                    window.cursor = Pos::default();
                    window.scroll = 0;
                } else if window.buffer > closing {
                    window.buffer -= 1;
                }
            }
        }
        self.input = InputState::default();
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

    /// The one-line question shown while a `c` substitute waits for an answer.
    pub fn replace_prompt(&self) -> Option<String> {
        let replacer = self.replacer.as_ref()?;
        Some(format!(
            "replace \"{}\" with \"{}\"?   (y)es (n)o (a)ll (q)uit   {}/{}",
            replacer.spec.pattern,
            replacer.spec.replacement,
            replacer.index + 1,
            replacer.matches.len()
        ))
    }

    /// Query text for the `/` prompt row.
    pub fn search_query(&self) -> &str {
        self.search.as_ref().map(|s| s.query.as_str()).unwrap_or("")
    }

    /// `/` or `?`, whichever started the search.
    pub fn search_prefix(&self) -> &'static str {
        match self.search.as_ref() {
            Some(search) if !search.forward => "?",
            _ => "/",
        }
    }

    /// `(index of the match under the cursor, total, whether counting stopped early)`.
    pub fn search_progress(&self) -> Option<(usize, usize, bool)> {
        let search = self.search.as_ref()?;
        if search.query.is_empty() {
            return None;
        }
        let case_sensitive = is_case_sensitive(&search.query);
        let (matches, truncated) = self.document().scan_matches(
            &search.query,
            case_sensitive,
            0,
            MATCH_SCAN_LINES,
            MATCH_COUNT_CAP,
        );
        let cursor = self.cursor();
        let index = matches
            .iter()
            .position(|pos| *pos == cursor)
            .map(|idx| idx + 1)
            .unwrap_or(0);
        Some((index, matches.len(), truncated))
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

    /// Bring every window's highlight state up to its first visible line.
    ///
    /// The walk is incremental: normal scrolling only tokenizes the lines that scrolled into or out
    /// of view. A jump so large that catching up would stall the frame restarts in the plain state
    /// instead, which is why a `G` on a huge file can look unstyled until the next scroll.
    fn sync_highlight(&mut self) {
        const MAX_CATCH_UP_LINES: usize = 20_000;
        let windows = &mut self.windows;
        let buffers = &self.buffers;

        for window in windows.iter_mut() {
            let Some(doc) = buffers.get(window.buffer) else {
                continue;
            };
            let language = Language::detect(doc.path());
            let scroll = window.scroll;

            if scroll < window.anchor_line || scroll - window.anchor_line > MAX_CATCH_UP_LINES {
                window.anchor_line = scroll;
                window.anchor_state = State::default();
            }
            while window.anchor_line < scroll {
                window.anchor_state = syntax::state_after(
                    language,
                    &doc.line(window.anchor_line),
                    window.anchor_state,
                );
                window.anchor_line += 1;
            }
            window.highlight = window.anchor_state;
        }
    }

    // ---- terminal -------------------------------------------------------

    /// Hand the screen to a shell. The editor keeps every buffer, cursor and undo stack untouched;
    /// only drawing stops until the shell is left.
    pub fn open_terminal(&mut self) {
        if self.terminal.is_some() {
            self.mode = Mode::Terminal;
            return;
        }
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        let rows = self.area.height.saturating_sub(1).max(1);
        let cols = self.area.width.max(1);
        match Session::spawn(&shell, rows, cols) {
            Ok(session) => {
                self.terminal = Some(session);
                self.mode = Mode::Terminal;
                self.message = None;
            }
            Err(err) => self.message = Some(format!("could not start {shell}: {err}")),
        }
    }

    pub fn terminal(&self) -> Option<&Session> {
        self.terminal.as_ref()
    }

    pub fn terminal_mut(&mut self) -> Option<&mut Session> {
        self.terminal.as_mut()
    }

    /// Come back to the editor. The shell session is discarded, not left running in the background.
    pub fn leave_terminal(&mut self, finished: bool) {
        self.terminal = None;
        self.mode = Mode::Normal;
        if finished {
            self.message = Some("shell exited".to_owned());
        }
    }

    /// Git state of the focused buffer.
    /// Whether a recovered edit is waiting for an answer.
    pub fn swap_prompt(&self) -> Option<&str> {
        self.swap_prompt.as_deref()
    }

    /// Mirror a dirty buffer to its swap file at most every few seconds. Called from the event loop
    /// on idle ticks.
    pub fn tick(&mut self) {
        let interval = std::time::Duration::from_secs(self.config.swap_seconds.max(1));
        if self.swap_prompt.is_some() || !self.document().is_dirty() {
            return;
        }
        let Some(path) = self.document().path().map(|path| path.to_path_buf()) else {
            return;
        };
        let due = self
            .last_swap_write
            .is_none_or(|last| last.elapsed() >= interval);
        if !due {
            return;
        }
        if swap::write(&path, &self.document().text()).is_ok() {
            self.last_swap_write = Some(std::time::Instant::now());
        }
    }

    fn answer_swap_prompt(&mut self, recover: bool) -> Outcome {
        let Some(text) = self.swap_prompt.take() else {
            return Outcome::Continue;
        };
        if let Some(path) = self.document().path().map(|path| path.to_path_buf()) {
            swap::clear(&path);
        }
        if recover {
            self.doc_mut().replace_all(&text);
            self.doc_mut().mark_dirty();
            let cursor = self.cursor();
            self.doc_mut().checkpoint(cursor);
            self.message = Some("recovered from swap file — :w to keep it".to_owned());
        } else {
            self.message = Some("swap file discarded".to_owned());
        }
        self.mode = Mode::Normal;
        Outcome::Continue
    }

    pub fn git_info(&self) -> &GitInfo {
        static EMPTY: GitInfo = GitInfo {
            branch: None,
            head_lines: None,
            marks: Vec::new(),
        };
        self.git.get(self.active()).unwrap_or(&EMPTY)
    }

    /// Git state of a specific buffer, for drawing that window's gutter.
    pub fn git_info_for(&self, buffer: usize) -> Option<&GitInfo> {
        self.git.get(buffer)
    }

    /// Commit message being typed, for the prompt row.
    pub fn commit_message(&self) -> &str {
        &self.commit_message
    }

    /// Ask git again: branch, HEAD content and marks.
    fn refresh_git(&mut self) {
        let index = self.active();
        let path = self.document().path().map(|path| path.to_path_buf());
        let text = self.document().text();
        if let Some(slot) = self.git.get_mut(index) {
            *slot = git::load(path.as_deref(), &text);
        }
    }

    /// Recompute marks from the cached HEAD content, without starting a process.
    fn recompute_marks(&mut self) {
        let index = self.active();
        let text = self.document().text();
        if let Some(info) = self.git.get_mut(index) {
            info.refresh_marks(&text);
        }
    }

    /// Language of the focused buffer, for the status line.
    pub fn language(&self) -> Language {
        self.config.language_for(self.document().path())
    }

    /// Width available for buffer text in the focused window.
    fn text_width(&self) -> usize {
        let gutter = crate::ui::gutter_width(self.document(), self.git_info().in_repo());
        (self.area.width as usize).saturating_sub(gutter)
    }

    /// Keep the cursor inside the visible window, scrolling as little as possible.
    pub fn follow_cursor(&mut self, visible: usize) {
        if visible == 0 {
            return;
        }
        let line = self.cursor().line;
        let margin = self.config.scroll_margin.min(visible / 2);
        if line < self.windows[self.focus].scroll + margin {
            self.windows[self.focus].scroll = line.saturating_sub(margin);
        } else if line + margin >= self.windows[self.focus].scroll + visible {
            self.windows[self.focus].scroll = (line + margin + 1).saturating_sub(visible);
        }
        if self.config.wrap && visible > 0 {
            let width = self.text_width();
            let line = self.cursor().line;
            let column = self.cursor().col;
            for _ in 0..200 {
                let rows = crate::ui::visible_rows(
                    self.document(),
                    self.windows[self.focus].scroll,
                    visible,
                    width,
                    self.config.tab_width,
                    true,
                );
                match rows
                    .iter()
                    .position(|(row_line, start)| *row_line == line && column >= *start)
                {
                    Some(index) if index + margin < visible => break,
                    _ => self.windows[self.focus].scroll += 1,
                }
            }
        }
        self.sync_highlight();
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
            match Document::open_with_limit(path, self.config.read_only_limit()) {
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

        let index = self.active();
        if self.is_blank_scratch() {
            self.buffers[index] = doc;
        } else {
            self.buffers.push(doc);
            let new_index = self.buffers.len() - 1;
            self.git.push(GitInfo::default());
            self.windows[self.focus].buffer = new_index;
        }
        self.refresh_git();
        self.input = InputState::default();
        self.windows[self.focus].cursor = Pos::default();
        self.windows[self.focus].scroll = 0;
        self.last_swap_write = None;
        if let Some(path) = self.document().path().map(|path| path.to_path_buf()) {
            if let Some(text) = swap::read(&path) {
                self.swap_prompt = Some(text);
                self.message = Some(
                    "unsaved changes from a previous session — (r)ecover (d)iscard".to_owned(),
                );
            }
        }
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
        if result.is_ok() {
            if let Some(path) = self.document().path().map(|path| path.to_path_buf()) {
                swap::clear(&path);
                self.last_swap_write = None;
            }
            self.refresh_git();
        }
        self.message = match result {
            Ok(()) => Some(format!("\"{}\" written", self.document().display_name())),
            Err(err) => Some(format!("save failed: {err}")),
        };
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if self.swap_prompt.is_some() {
            return match key.code {
                KeyCode::Char('r') => self.answer_swap_prompt(true),
                KeyCode::Char('d') | KeyCode::Esc => self.answer_swap_prompt(false),
                _ => Outcome::Continue,
            };
        }
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Help => self.handle_help(key),
            Mode::Command => self.handle_command_line(key),
            Mode::Search => self.handle_search(key),
            Mode::ReplaceConfirm => self.handle_replace_confirm(key),
            Mode::WindowCmd => self.handle_window_command(key),
            Mode::CommitMsg => self.handle_commit_message(key),
            Mode::Terminal => Outcome::Continue,
            Mode::Visual => self.handle_visual(key),
            Mode::FindFile => self.handle_find_file(key),
            Mode::Palette => self.handle_palette(key),
        }
    }

    // ---- normal mode ---------------------------------------------------

    fn handle_normal(&mut self, key: KeyEvent) -> Outcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(action) = self.keymap.action_for(key.code, true) {
                return self.run_action(action);
            }
            return Outcome::Continue;
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

        if self.input.awaiting_register {
            self.input.awaiting_register = false;
            if let KeyCode::Char(name) = key.code {
                self.input.register = Some(name);
            }
            return Outcome::Continue;
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
        let cursor = self.cursor();

        // an operator keeps whatever count was typed (`2d…`) and waits for its motion
        if let Some(Action::Operator(op)) = self.keymap.action_for(key.code, false) {
            self.input.operator = Some(op);
            return;
        }

        // motions come from the table, with the count the user typed
        if let Some(motion) = keymap::motion_for(key.code) {
            let target = self.apply_motion(cursor, motion, count);
            self.set_cursor(target);
            return;
        }

        // one key, one self-contained action: the table owns these
        if let Some(action) = self.keymap.action_for(key.code, false) {
            self.input.count = None;
            self.run_action(action);
            return;
        }
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => self.motion(|doc, c| doc.move_left(c, count)),
            KeyCode::Char('j') | KeyCode::Down => self.motion(|doc, c| doc.move_down(c, count)),
            KeyCode::Char('k') | KeyCode::Up => self.motion(|doc, c| doc.move_up(c, count)),
            KeyCode::Char('l') | KeyCode::Right => self.motion(|doc, c| doc.move_right(c, count)),
            KeyCode::Char('w') => self.motion(|doc, c| doc.move_word_forward(c, count)),
            KeyCode::Char('b') => self.motion(|doc, c| doc.move_word_backward(c, count)),
            KeyCode::Home | KeyCode::Char('0') => {
                self.set_cursor(Pos::new(cursor.line, 0));
            }
            KeyCode::End | KeyCode::Char('$') => self.motion(|doc, c| doc.move_line_end(c)),
            KeyCode::Char('G') => {
                let line = if had_count {
                    count
                } else {
                    self.document().line_count()
                };
                self.jump_to_line(line);
            }
            KeyCode::Char('g') => self.input.g_prefix = true,
            KeyCode::Char('"') => self.input.awaiting_register = true,
            KeyCode::Char('n') => self.search_next(),
            KeyCode::Char('N') => self.search_previous(),
            _ => {}
        }
    }

    /// Execute one table-driven action.
    fn run_action(&mut self, action: Action) -> Outcome {
        match action {
            Action::Quit => return self.try_quit(),
            Action::Palette => self.open_palette(),
            Action::WindowCommand => self.mode = Mode::WindowCmd,
            Action::Terminal => self.open_terminal(),
            Action::FindFile => self.open_find_file(),
            Action::NextBuffer => self.next_buffer(),
            Action::PrevBuffer => self.prev_buffer(),
            Action::Undo => self.undo(),
            Action::Redo => self.redo(),
            Action::Visual => self.start_visual(),
            Action::JoinLines => self.join_lines(),
            Action::MatchBracket => self.jump_to_matching_bracket(),
            Action::SearchForward => self.start_search(true),
            Action::SearchBackward => self.start_search(false),
            Action::Operator(op) => self.input.operator = Some(op),
            Action::Insert(kind) => self.begin_insert(kind),
            Action::DeleteChar => self.delete_under_cursor(1),
            Action::Paste(before) => self.paste(before),
            Action::Repeat => self.repeat_last_change(),
        }
        Outcome::Continue
    }

    /// `.`: apply the last change again at the cursor.
    fn repeat_last_change(&mut self) {
        let Some(repeat) = self.last_change.clone() else {
            self.message = Some("nothing to repeat yet".to_owned());
            return;
        };
        match repeat {
            Repeat::Operator(op, motion) => {
                let Some((start, end)) = self.operator_range(key_for_motion(motion), 1) else {
                    return;
                };
                self.execute_operator(op, start, end);
            }
            Repeat::DeleteChar => self.delete_under_cursor(1),
            Repeat::Visual(op, width) => {
                let start = self.cursor();
                let mut end = start;
                for _ in 0..width {
                    let next = Pos::new(end.line, end.col + 1);
                    if end.col < self.document().line_len(end.line) {
                        end = next;
                    } else if end.line + 1 < self.document().line_count() {
                        end = Pos::new(end.line + 1, 0);
                    } else {
                        break;
                    }
                }
                self.execute_operator(op, start, end);
            }
            Repeat::Paste(before) => self.paste(before),
            Repeat::Insert(kind, text) => {
                let line = self.cursor().line;
                match kind {
                    InsertKind::LineEnd => {
                        let end = self.document().line_len(line);
                        self.set_cursor(Pos::new(line, end));
                    }
                    InsertKind::LineStart => self.set_cursor(Pos::new(line, 0)),
                    InsertKind::After => self.move_cursor_right(1),
                    InsertKind::LineBelow | InsertKind::LineAbove => {
                        let at = if kind == InsertKind::LineBelow {
                            Pos::new(line, self.document().line_len(line))
                        } else {
                            Pos::new(line, 0)
                        };
                        self.set_cursor(at);
                        self.checkpoint();
                        self.edit(|doc, cursor| doc.insert_newline(cursor));
                        if kind == InsertKind::LineAbove {
                            self.set_cursor(Pos::new(line, 0));
                        }
                    }
                    InsertKind::Here => {}
                }
                self.checkpoint();
                self.edit(|doc, cursor| {
                    for ch in text.chars() {
                        if ch == '\n' {
                            doc.insert_newline(cursor);
                        } else {
                            doc.insert_char(cursor, ch);
                        }
                    }
                });
            }
        }
    }

    /// Enter insert mode at the place the action asks for.
    fn begin_insert(&mut self, kind: InsertKind) {
        self.insert_kind = kind;
        let line = self.cursor().line;
        match kind {
            InsertKind::Here => self.enter_insert(),
            InsertKind::After => {
                self.move_cursor_right(1);
                self.enter_insert();
            }
            InsertKind::LineStart => {
                self.set_cursor(Pos::new(line, 0));
                self.enter_insert();
            }
            InsertKind::LineEnd => {
                let end = self.document().line_len(line);
                self.set_cursor(Pos::new(line, end));
                self.enter_insert();
            }
            InsertKind::LineBelow => {
                self.checkpoint();
                self.insert_started = true;
                let end = self.document().line_len(line);
                self.set_cursor(Pos::new(line, end));
                self.edit(|doc, cursor| doc.insert_newline(cursor));
                self.enter_insert();
            }
            InsertKind::LineAbove => {
                self.checkpoint();
                self.insert_started = true;
                self.set_cursor(Pos::new(line, 0));
                self.edit(|doc, cursor| doc.insert_newline(cursor));
                self.set_cursor(Pos::new(line, 0));
                self.enter_insert();
            }
        }
    }

    fn enter_insert(&mut self) {
        // The checkpoint is taken lazily, on the first key that actually changes the text, so an
        // insert session that types nothing leaves no useless undo step behind.
        self.insert_started = false;
        self.mode = Mode::Insert;
    }

    fn checkpoint(&mut self) {
        let cursor = self.cursor();
        self.doc_mut().checkpoint(cursor);
    }

    fn undo(&mut self) {
        let cursor = self.cursor();
        match self.doc_mut().undo(cursor) {
            Some(restored) => {
                self.message = None;
                self.set_cursor(restored);
                self.clamp_cursor();
            }
            None => self.message = Some("already at the oldest change".to_owned()),
        }
    }

    fn redo(&mut self) {
        let cursor = self.cursor();
        match self.doc_mut().redo(cursor) {
            Some(restored) => {
                self.message = None;
                self.set_cursor(restored);
                self.clamp_cursor();
            }
            None => self.message = Some("nothing to redo".to_owned()),
        }
    }

    fn jump_to_line(&mut self, line: usize) {
        let target = line
            .saturating_sub(1)
            .min(self.document().line_count().saturating_sub(1));
        self.set_cursor(Pos::new(target, 0));
        self.clamp_cursor();
    }

    fn move_cursor_right(&mut self, count: usize) {
        let cursor = self.cursor();
        let col = (cursor.col + count).min(self.document().line_len(cursor.line));
        self.set_cursor(Pos::new(cursor.line, col));
    }

    fn delete_under_cursor(&mut self, count: usize) {
        if self.document().is_read_only() {
            self.message = Some("read-only: file is larger than 50 MB".to_owned());
            return;
        }
        let cursor = self.cursor();
        let at_last_char = cursor.col >= self.document().line_len(cursor.line)
            && cursor.line + 1 >= self.document().line_count();
        if at_last_char {
            return;
        }
        self.checkpoint();
        let mut removed = String::new();
        for _ in 0..count.max(1) {
            let cursor = self.cursor();
            let len = self.document().line_len(cursor.line);
            let end = if cursor.col < len {
                Pos::new(cursor.line, cursor.col + 1)
            } else if cursor.line + 1 < self.document().line_count() {
                Pos::new(cursor.line + 1, 0)
            } else {
                break;
            };
            removed.push_str(&self.doc_mut().remove(cursor, end));
            self.set_cursor(cursor);
        }
        if !removed.is_empty() {
            self.register = Some(Register {
                text: removed,
                linewise: false,
            });
            self.last_change = Some(Repeat::DeleteChar);
        }
    }

    fn apply_operator(&mut self, op: Operator, key: KeyEvent) {
        if self.document().is_read_only() {
            self.message = Some("read-only: file is larger than 50 MB".to_owned());
            self.input = InputState::default();
            return;
        }
        if key.code == KeyCode::Esc {
            self.input = InputState::default();
            return;
        }

        // second half of a text object: `diw`, `ca"`, ...
        if let Some(inner) = self.input.text_object {
            self.input.text_object = None;
            let KeyCode::Char(kind) = key.code else {
                return;
            };
            if let Some((start, end)) = self.text_object(inner, kind) {
                self.execute_operator(op, start, end);
            }
            return;
        }
        // first half: keep the operator pending and remember which flavour was asked for
        if let KeyCode::Char(kind @ ('i' | 'a')) = key.code {
            self.input.text_object = Some(kind == 'i');
            return;
        }

        self.input.operator = None;
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
        if let Some(motion) = keymap::motion_for(key.code) {
            self.last_change = Some(Repeat::Operator(op, motion));
        }
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

        self.execute_operator(op, start, end);
    }

    /// Apply `op` to a half-open range, honouring any register picked with `"`.
    fn execute_operator(&mut self, op: Operator, start: Pos, end: Pos) {
        if start >= end {
            return;
        }
        let target = self.input.register.take();
        let text = self.document().slice(start, end);
        match op {
            Operator::Yank => {
                self.store_register(target, text, false);
                self.set_cursor(start);
                self.clamp_cursor();
            }
            Operator::Delete => {
                self.checkpoint();
                self.doc_mut().remove(start, end);
                self.store_register(target, text, false);
                self.set_cursor(start);
                self.clamp_cursor();
            }
            Operator::Change => {
                self.checkpoint();
                self.doc_mut().remove(start, end);
                self.store_register(target, text, false);
                self.set_cursor(start);
                self.enter_insert();
            }
        }
    }

    /// Apply a table-defined motion to a cursor.
    fn apply_motion(&self, cursor: Pos, motion: Motion, count: usize) -> Pos {
        let doc = self.document();
        match motion {
            Motion::Left => doc.move_left(cursor, count),
            Motion::Right => doc.move_right(cursor, count),
            Motion::Up => doc.move_up(cursor, count),
            Motion::Down => doc.move_down(cursor, count),
            Motion::WordForward => doc.move_word_forward(cursor, count),
            Motion::WordBackward => doc.move_word_backward(cursor, count),
            Motion::WordEnd => doc.move_word_end(cursor, count, false),
            Motion::WordEndBig => doc.move_word_end(cursor, count, true),
            Motion::LineStart => Pos::new(cursor.line, 0),
            Motion::LineEnd => doc.move_line_end(cursor),
        }
    }

    /// The half-open range an operator would touch if followed by this motion.
    fn operator_range(&self, code: KeyCode, count: usize) -> Option<(Pos, Pos)> {
        let motion = keymap::motion_for(code)?;
        let start = self.cursor();
        let target = self.apply_motion(start, motion, count);
        let (a, b) = ordered(start, target);
        if motion.inclusive() {
            Some((a, Pos::new(b.line, b.col + 1)))
        } else {
            Some((a, b))
        }
    }

    fn apply_linewise(&mut self, op: Operator, count: usize) {
        if self.document().is_read_only() {
            self.message = Some("read-only: file is larger than 50 MB".to_owned());
            return;
        }
        let line = self.cursor().line;
        let available = self.document().line_count().saturating_sub(line);
        let n = count.min(available.max(1));

        let lines: Vec<String> = (0..n).map(|i| self.document().line(line + i)).collect();
        let text = lines.join("\n");
        let target = self.input.register.take();
        if op != Operator::Yank {
            self.checkpoint();
        }

        match op {
            Operator::Yank => {
                self.store_register(target, text, true);
            }
            Operator::Delete => {
                for _ in 0..n {
                    self.doc_mut().delete_line(line);
                }
                self.store_register(target, text, true);
            }
            Operator::Change => {
                for _ in 0..n {
                    self.doc_mut().delete_line(line);
                }
                self.doc_mut().insert_lines(line, &[String::new()]);
                self.set_cursor(Pos::new(line, 0));
                self.store_register(target, text, true);
                self.enter_insert();
            }
        }
    }

    fn paste(&mut self, before: bool) {
        if self.document().is_read_only() {
            self.message = Some("read-only: file is larger than 50 MB".to_owned());
            return;
        }
        let target = self.input.register.take();
        let Some(register) = self.take_register(target) else {
            // nothing yanked in this session: fall back to the system clipboard
            match clipboard::paste() {
                Some(text) => {
                    self.checkpoint();
                    let pos = self.cursor();
                    self.doc_mut().insert_at(pos, &text);
                    self.set_cursor(advance_pos(pos, &text));
                    self.clamp_cursor();
                }
                None => self.message = Some("nothing to paste yet".to_owned()),
            }
            return;
        };
        self.checkpoint();
        self.last_change = Some(Repeat::Paste(before));
        if register.linewise {
            let lines: Vec<String> = register.text.split('\n').map(str::to_owned).collect();
            let at = if before {
                self.cursor().line
            } else {
                self.cursor().line + 1
            };
            self.doc_mut().insert_lines(at, &lines);
            self.set_cursor(Pos::new(at, 0));
            self.clamp_cursor();
        } else {
            let mut pos = self.cursor();
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
            self.set_cursor(Pos::new(line, col));
            self.clamp_cursor();
        }
    }

    // ---- search --------------------------------------------------------

    fn start_search(&mut self, forward: bool) {
        self.search = Some(Search {
            query: String::new(),
            origin: self.cursor(),
            origin_scroll: self.windows[self.focus].scroll,
            forward,
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
            self.set_cursor(search.origin);
            self.windows[self.focus].scroll = search.origin_scroll;
        }
        self.mode = Mode::Normal;
    }

    /// Jump to the first match from where the search started, wrapping once.
    fn update_search_cursor(&mut self) {
        let Some(search) = self.search.clone() else {
            return;
        };
        if search.query.is_empty() {
            self.set_cursor(search.origin);
            self.message = None;
            return;
        }
        let case_sensitive = is_case_sensitive(&search.query);
        let found = if search.forward {
            self.document()
                .find_forward(search.origin, &search.query, case_sensitive)
                .or_else(|| {
                    self.document()
                        .find_forward(Pos::new(0, 0), &search.query, case_sensitive)
                })
        } else {
            // `?` looks behind the starting point, not at it
            let behind = self.previous_position(search.origin).and_then(|from| {
                self.document()
                    .find_backward(from, &search.query, case_sensitive)
            });
            behind.or_else(|| {
                let last = self.document().line_count().saturating_sub(1);
                let end = Pos::new(last, self.document().line_len(last));
                self.document()
                    .find_backward(end, &search.query, case_sensitive)
            })
        };
        self.report_match(found, &search.query);
    }

    fn search_next(&mut self) {
        let forward = self.search.as_ref().is_none_or(|search| search.forward);
        self.repeat_search(forward);
    }

    fn search_previous(&mut self) {
        let forward = self.search.as_ref().is_none_or(|search| search.forward);
        self.repeat_search(!forward);
    }

    /// `n`/`N`: continue the last search, wrapping around the buffer.
    fn repeat_search(&mut self, forward: bool) {
        let Some(query) = self.search.as_ref().map(|search| search.query.clone()) else {
            self.message = Some("no search yet — press / to search".to_owned());
            return;
        };
        if query.is_empty() {
            return;
        }
        let case_sensitive = is_case_sensitive(&query);
        let cursor = self.cursor();
        let found = if forward {
            self.document()
                .find_forward(self.next_position(cursor), &query, case_sensitive)
                .or_else(|| {
                    self.document()
                        .find_forward(Pos::new(0, 0), &query, case_sensitive)
                })
        } else {
            let from = self.previous_position(cursor);
            let found =
                from.and_then(|from| self.document().find_backward(from, &query, case_sensitive));
            found.or_else(|| {
                let last = self.document().line_count().saturating_sub(1);
                let end = Pos::new(last, self.document().line_len(last));
                self.document().find_backward(end, &query, case_sensitive)
            })
        };
        self.report_match(found, &query);
    }

    fn report_match(&mut self, found: Option<Pos>, query: &str) {
        match found {
            Some(pos) => {
                self.set_cursor(pos);
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
            Some(Pos::new(
                pos.line - 1,
                self.document().line_len(pos.line - 1),
            ))
        } else {
            None
        }
    }

    // ---- substitute ----------------------------------------------------

    fn substitute(&mut self, spec: Substitute) -> Outcome {
        let case_sensitive = is_case_sensitive(&spec.pattern);
        let from_line = if spec.whole_file {
            0
        } else {
            self.cursor().line
        };
        let max_lines = if spec.whole_file {
            self.document().line_count()
        } else {
            1
        };
        let (scan, _) = self.document().scan_matches(
            &spec.pattern,
            case_sensitive,
            from_line,
            max_lines,
            usize::MAX / 2,
        );
        let matches: Vec<Pos> = if spec.global {
            scan
        } else {
            // without /g only the first match of each line counts
            let mut firsts: Vec<Pos> = Vec::new();
            for pos in scan {
                if firsts.last().is_none_or(|prev| prev.line != pos.line) {
                    firsts.push(pos);
                }
            }
            firsts
        };

        if matches.is_empty() {
            self.message = Some(format!("pattern not found: {}", spec.pattern));
            return Outcome::Continue;
        }

        if spec.confirm {
            self.set_cursor(matches[0]);
            self.message = None;
            self.replacer = Some(Replacer {
                spec,
                matches,
                index: 0,
                approved: Vec::new(),
            });
            self.mode = Mode::ReplaceConfirm;
            return Outcome::Continue;
        }

        let total = matches.len();
        let done = self.apply_replacements(&spec, &matches);
        self.set_cursor(matches[0]);
        self.message = Some(format!("replaced {done} of {total} occurrences"));
        Outcome::Continue
    }

    /// Apply every replacement by rebuilding the buffer text once instead of editing the rope per
    /// match: a whole-file substitute on a large file is thousands of edits otherwise.
    fn apply_replacements(&mut self, spec: &Substitute, positions: &[Pos]) -> usize {
        if positions.is_empty() {
            return 0;
        }
        self.checkpoint();
        let width = spec.pattern.chars().count();
        let text = self.document().text();

        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(positions.len());
        let mut line_cursor = usize::MAX;
        let mut line_start_byte = 0;
        let mut line_text = String::new();
        for pos in positions {
            if pos.line != line_cursor {
                line_cursor = pos.line;
                line_text = self.document().line(pos.line);
                let line_start_char = self.document().rope().line_to_char(pos.line);
                line_start_byte = self.document().rope().char_to_byte(line_start_char);
            }
            let start = line_start_byte + byte_of_column(&line_text, pos.col);
            let end = line_start_byte + byte_of_column(&line_text, pos.col + width);
            ranges.push((start, end));
        }

        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for (start, end) in ranges {
            if start < last || end > text.len() {
                continue;
            }
            out.push_str(&text[last..start]);
            out.push_str(&spec.replacement);
            last = end;
        }
        out.push_str(&text[last..]);

        self.doc_mut().replace_all(&out);
        positions.len()
    }

    fn handle_replace_confirm(&mut self, key: KeyEvent) -> Outcome {
        let Some(mut replacer) = self.replacer.take() else {
            self.mode = Mode::Normal;
            return Outcome::Continue;
        };

        match key.code {
            KeyCode::Char('y') => {
                if let Some(pos) = replacer.matches.get(replacer.index).copied() {
                    replacer.approved.push(pos);
                }
                replacer.index += 1;
            }
            KeyCode::Char('n') => replacer.index += 1,
            KeyCode::Char('a') => {
                replacer
                    .approved
                    .extend_from_slice(&replacer.matches[replacer.index..]);
                replacer.index = replacer.matches.len();
            }
            KeyCode::Char('q') | KeyCode::Esc => replacer.index = replacer.matches.len(),
            _ => {}
        }

        if replacer.index < replacer.matches.len() {
            let next = replacer.matches[replacer.index];
            self.set_cursor(next);
            self.replacer = Some(replacer);
        } else {
            self.replacer = Some(replacer);
            self.finish_replace_confirm();
        }
        Outcome::Continue
    }

    fn finish_replace_confirm(&mut self) {
        let Some(replacer) = self.replacer.take() else {
            return;
        };
        self.mode = Mode::Normal;
        let total = replacer.matches.len();
        let done = self.apply_replacements(&replacer.spec, &replacer.approved);
        if let Some(pos) = replacer
            .approved
            .first()
            .or_else(|| replacer.matches.first())
        {
            self.set_cursor(*pos);
        }
        self.message = Some(format!("replaced {done} of {total} occurrences"));
    }

    // ---- file picker ----------------------------------------------------

    /// List the project once, then filter in memory as the user types.
    pub fn open_find_file(&mut self) {
        let root = self
            .document()
            .path()
            .and_then(|path| path.parent())
            .map(|dir| dir.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
        self.finder_files = finder::collect(&root, 5000);
        self.finder_root = Some(root);
        self.finder_query.clear();
        self.finder_selected = 0;
        self.mode = Mode::FindFile;
        self.message = None;
    }

    /// Files matching the query, best first.
    pub fn finder_matches(&self) -> Vec<&str> {
        if self.finder_query.is_empty() {
            return self.finder_files.iter().map(String::as_str).collect();
        }
        let mut scored: Vec<(i32, &str)> = self
            .finder_files
            .iter()
            .filter_map(|file| {
                let name = file.rsplit('/').next().unwrap_or(file);
                let by_name = command::fuzzy_score(&self.finder_query, name);
                let by_path = command::fuzzy_score(&self.finder_query, file).map(|score| score / 2);
                let score = match (by_name, by_path) {
                    (Some(a), Some(b)) => a.max(b),
                    (a, b) => a.or(b)?,
                };
                Some((score, file.as_str()))
            })
            .collect();
        scored.sort_by_key(|(score, file)| (std::cmp::Reverse(*score), file.to_string()));
        scored.into_iter().map(|(_, file)| file).collect()
    }

    pub fn finder_query(&self) -> &str {
        &self.finder_query
    }

    pub fn finder_selected(&self) -> usize {
        self.finder_selected
    }

    fn handle_find_file(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let chosen = self
                    .finder_matches()
                    .get(self.finder_selected)
                    .map(|file| (*file).to_owned());
                self.mode = Mode::Normal;
                if let (Some(root), Some(file)) = (self.finder_root.clone(), chosen) {
                    self.open(root.join(file));
                }
            }
            KeyCode::Up => self.finder_selected = self.finder_selected.saturating_sub(1),
            KeyCode::Down => {
                let last = self.finder_matches().len().saturating_sub(1);
                self.finder_selected = (self.finder_selected + 1).min(last);
            }
            KeyCode::Backspace => {
                self.finder_query.pop();
                self.finder_selected = 0;
            }
            KeyCode::Char(ch) => {
                self.finder_query.push(ch);
                self.finder_selected = 0;
            }
            _ => {}
        }
        Outcome::Continue
    }

    // ---- visual mode ----------------------------------------------------

    pub fn start_visual(&mut self) {
        self.visual_anchor = Some(self.cursor());
        self.mode = Mode::Visual;
    }

    /// The selected range, half-open, in buffer coordinates.
    pub fn selection(&self) -> Option<(Pos, Pos)> {
        if self.mode != Mode::Visual {
            return None;
        }
        let anchor = self.visual_anchor?;
        let cursor = self.cursor();
        let (a, b) = ordered(anchor, cursor);
        Some((a, Pos::new(b.line, b.col + 1)))
    }

    fn handle_visual(&mut self, key: KeyEvent) -> Outcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return self.try_quit();
        }
        let count = 1;
        match key.code {
            KeyCode::Esc | KeyCode::Char('v') => self.leave_visual(),
            KeyCode::Char('o') => {
                let cursor = self.cursor();
                if let Some(anchor) = self.visual_anchor.replace(cursor) {
                    self.set_cursor(anchor);
                }
            }
            KeyCode::Char('d') => self.visual_operator(Operator::Delete),
            KeyCode::Char('y') => self.visual_operator(Operator::Yank),
            KeyCode::Char('c') => self.visual_operator(Operator::Change),
            KeyCode::Char('"') => self.input.awaiting_register = true,
            _ if keymap::motion_for(key.code).is_some() => {
                let motion = keymap::motion_for(key.code).expect("checked above");
                let target = self.apply_motion(self.cursor(), motion, count);
                self.set_cursor(target);
            }
            _ => {}
        }
        Outcome::Continue
    }

    fn leave_visual(&mut self) {
        self.visual_anchor = None;
        self.mode = Mode::Normal;
    }

    fn visual_operator(&mut self, op: Operator) {
        let Some((start, end)) = self.selection() else {
            self.leave_visual();
            return;
        };
        let width = self.document().slice(start, end).chars().count();
        self.leave_visual();
        self.execute_operator(op, start, end);
        self.last_change = Some(Repeat::Visual(op, width));
    }

    // ---- registers ------------------------------------------------------

    fn store_register(&mut self, name: Option<char>, text: String, linewise: bool) {
        // yanking also reaches the system clipboard, so it survives leaving the terminal
        let _ = clipboard::copy(&text);
        let register = Register { text, linewise };
        match name {
            Some(name) => {
                self.named_registers.insert(name, register);
            }
            None => self.register = Some(register),
        }
    }

    fn take_register(&mut self, name: Option<char>) -> Option<Register> {
        match name {
            Some(name) => self.named_registers.get(&name).cloned(),
            None => self.register.clone(),
        }
    }

    // ---- extra motions --------------------------------------------------

    /// `J`: pull the next line up, leaving a single space when both sides have text.
    fn join_lines(&mut self) {
        if self.document().is_read_only() {
            self.message = Some("read-only: file is larger than 50 MB".to_owned());
            return;
        }
        let line = self.cursor().line;
        if line + 1 >= self.document().line_count() {
            self.message = Some("nothing to join below".to_owned());
            return;
        }
        self.checkpoint();
        let left_len = self.document().line_len(line);
        let right_len = self.document().line_len(line + 1);
        let needs_space = left_len > 0 && right_len > 0;
        self.doc_mut()
            .remove(Pos::new(line, left_len), Pos::new(line + 1, 0));
        if needs_space {
            self.doc_mut().insert_at(Pos::new(line, left_len), " ");
        }
        let column = if needs_space { left_len + 1 } else { left_len };
        self.set_cursor(Pos::new(line, column));
        self.clamp_cursor();
    }

    /// `%`: jump between a bracket and its partner, scanning across lines.
    fn jump_to_matching_bracket(&mut self) {
        let cursor = self.cursor();
        let Some(open) = self.document().char_at_pos(cursor) else {
            self.message = Some("no bracket under the cursor".to_owned());
            return;
        };
        let (pair, forward) = match open {
            '(' => (')', true),
            '[' => (']', true),
            '{' => ('}', true),
            ')' => ('(', false),
            ']' => ('[', false),
            '}' => ('{', false),
            _ => {
                self.message = Some("no bracket under the cursor".to_owned());
                return;
            }
        };
        // whichever way we scan, brackets of the same kind as the one under the cursor nest, and the
        // partner kind is what we are looking for
        let (nesting, target) = (open, pair);

        let mut depth = 0i32;
        let mut pos = cursor;
        while let Some(next) = self.step_position(pos, forward) {
            pos = next;
            match self.document().char_at_pos(pos) {
                Some(ch) if ch == target => {
                    if depth == 0 {
                        self.set_cursor(pos);
                        return;
                    }
                    depth -= 1;
                }
                Some(ch) if ch == nesting => depth += 1,
                _ => {}
            }
            if depth > 200 {
                break;
            }
        }
        self.message = Some(format!("no matching {target}"));
    }

    /// One character forward or backward through the buffer.
    fn step_position(&self, pos: Pos, forward: bool) -> Option<Pos> {
        let doc = self.document();
        if forward {
            if pos.col + 1 < doc.line_len(pos.line) {
                Some(Pos::new(pos.line, pos.col + 1))
            } else if pos.line + 1 < doc.line_count() {
                Some(Pos::new(pos.line + 1, 0))
            } else {
                None
            }
        } else if pos.col > 0 {
            Some(Pos::new(pos.line, pos.col - 1))
        } else if pos.line > 0 {
            let previous = pos.line - 1;
            Some(Pos::new(previous, doc.line_len(previous).saturating_sub(1)))
        } else {
            None
        }
    }

    // ---- text objects ---------------------------------------------------

    /// Range for `i`/`a` plus a character, on the cursor's line. Multi-line nesting is out of scope.
    fn text_object(&self, inner: bool, kind: char) -> Option<(Pos, Pos)> {
        let cursor = self.cursor();
        let chars: Vec<char> = self.document().line(cursor.line).chars().collect();
        if chars.is_empty() {
            return None;
        }

        if kind == 'w' {
            let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
            if !chars.get(cursor.col).copied().is_some_and(is_word) {
                return None;
            }
            let mut start = cursor.col;
            while start > 0 && is_word(chars[start - 1]) {
                start -= 1;
            }
            let mut end = cursor.col;
            while end < chars.len() && is_word(chars[end]) {
                end += 1;
            }
            if !inner {
                let before = end;
                while end < chars.len() && chars[end].is_whitespace() {
                    end += 1;
                }
                if end == before {
                    while start > 0 && chars[start - 1].is_whitespace() {
                        start -= 1;
                    }
                }
            }
            return Some((Pos::new(cursor.line, start), Pos::new(cursor.line, end)));
        }

        let (open, close) = match kind {
            '"' => ('"', '"'),
            '\'' => ('\'', '\''),
            '`' => ('`', '`'),
            '(' | ')' => ('(', ')'),
            '[' | ']' => ('[', ']'),
            '{' | '}' => ('{', '}'),
            '<' | '>' => ('<', '>'),
            _ => return None,
        };
        let (open_at, close_at) = enclosing_pair(&chars, cursor.col, open, close)?;
        let (start, end) = if inner {
            (open_at + 1, close_at)
        } else {
            (open_at, close_at + 1)
        };
        Some((Pos::new(cursor.line, start), Pos::new(cursor.line, end)))
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
            self.insert_typed.clear();
        }
        if let KeyCode::Char(ch) = key.code {
            self.insert_typed.push(ch);
        } else if key.code == KeyCode::Enter {
            self.insert_typed.push('\n');
        } else if key.code == KeyCode::Tab {
            self.insert_typed.push('\t');
        }
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                if !self.insert_typed.is_empty() {
                    let text = std::mem::take(&mut self.insert_typed);
                    self.last_change = Some(Repeat::Insert(self.insert_kind, text));
                }
                self.recompute_marks();
                let cursor = self.cursor();
                if cursor.col > 0 {
                    let back = Pos::new(cursor.line, cursor.col - 1);
                    self.set_cursor(back);
                }
                self.clamp_cursor();
            }
            KeyCode::Enter => self.edit(|doc, cursor| doc.insert_newline(cursor)),
            KeyCode::Tab => self.edit(|doc, cursor| doc.insert_char(cursor, '\t')),
            KeyCode::Backspace => self.edit(|doc, cursor| doc.backspace(cursor)),
            KeyCode::Delete => self.edit(|doc, cursor| doc.delete_forward(cursor)),
            // arrows stay inside the line, like normal-mode `h`/`l`
            KeyCode::Left => self.motion(|doc, cursor| doc.move_left(cursor, 1)),
            KeyCode::Right => {
                let cursor = self.cursor();
                let col = (cursor.col + 1).min(self.document().line_len(cursor.line));
                self.set_cursor(Pos::new(cursor.line, col));
            }
            KeyCode::Up => self.motion(|doc, cursor| doc.move_up(cursor, 1)),
            KeyCode::Down => self.motion(|doc, cursor| doc.move_down(cursor, 1)),
            KeyCode::Char(c) => self.edit(|doc, cursor| doc.insert_char(cursor, c)),
            _ => {}
        }
        Outcome::Continue
    }

    // ---- commit message -------------------------------------------------

    fn start_commit(&mut self) {
        if !self.git_info().in_repo() {
            self.message = Some("not a git repository".to_owned());
            return;
        }
        if self.document().is_dirty() {
            self.message = Some("save the file first — :w".to_owned());
            return;
        }
        self.commit_message.clear();
        self.mode = Mode::CommitMsg;
    }

    fn handle_commit_message(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => {
                self.commit_message.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.commit(),
            KeyCode::Backspace => {
                self.commit_message.pop();
            }
            KeyCode::Char(ch) => self.commit_message.push(ch),
            _ => {}
        }
        Outcome::Continue
    }

    fn commit(&mut self) {
        let text = std::mem::take(&mut self.commit_message);
        self.mode = Mode::Normal;
        let text = text.trim().to_owned();
        if text.is_empty() {
            self.message = Some("commit message is empty".to_owned());
            return;
        }
        let path = self.document().path().map(|path| path.to_path_buf());
        self.message = Some(match git::commit(path.as_deref(), &text) {
            Ok(summary) => summary,
            Err(err) => err,
        });
        if self
            .message
            .as_deref()
            .is_some_and(|line| !line.contains("failed") && !line.contains("empty"))
        {
            self.refresh_git();
        }
    }

    // ---- window commands -----------------------------------------------

    fn handle_window_command(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Char(direction @ ('h' | 'j' | 'k' | 'l')) => {
                self.mode = Mode::Normal;
                self.focus_direction(direction);
            }
            KeyCode::Char('w') => {
                self.mode = Mode::Normal;
                self.focus_next_window();
            }
            KeyCode::Char('c') => {
                self.mode = Mode::Normal;
                self.close_window();
            }
            KeyCode::Char('o') => {
                self.mode = Mode::Normal;
                self.only_window();
            }
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            _ => self.mode = Mode::Normal,
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
        if command::looks_like_substitute(&line) {
            return match command::parse_substitute(&line) {
                Ok(spec) => self.substitute(spec),
                Err(message) => {
                    self.message = Some(message);
                    Outcome::Continue
                }
            };
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
            "substitute" => {
                self.message = Some(command::SUBSTITUTE_USAGE.to_owned());
                Outcome::Continue
            }
            "search" => {
                self.start_search(true);
                Outcome::Continue
            }
            "search-backward" => {
                self.start_search(false);
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
            "find-file" => {
                self.open_find_file();
                Outcome::Continue
            }
            "terminal" => {
                self.open_terminal();
                Outcome::Continue
            }
            "git-commit" => {
                self.start_commit();
                Outcome::Continue
            }
            "git-refresh" => {
                self.refresh_git();
                self.message = Some(match self.git_info().branch.as_deref() {
                    Some(branch) => format!("git refreshed (branch {branch})"),
                    None => "not a git repository".to_owned(),
                });
                Outcome::Continue
            }
            "split" => {
                self.split_window(SplitDir::Stacked);
                Outcome::Continue
            }
            "vsplit" => {
                self.split_window(SplitDir::SideBySide);
                Outcome::Continue
            }
            "resize" => {
                let delta = arg.trim();
                let parsed = delta
                    .strip_prefix('+')
                    .map(str::parse::<i16>)
                    .unwrap_or_else(|| delta.parse::<i16>());
                match parsed {
                    Ok(value) if delta.starts_with('+') => self.resize_window(value),
                    Ok(value) if delta.starts_with('-') => self.resize_window(-value),
                    Ok(value) => self.resize_window(value),
                    Err(_) => self.message = Some("usage: :resize +5  or  :resize -5".to_owned()),
                }
                Outcome::Continue
            }
            "close-window" => {
                self.close_window();
                Outcome::Continue
            }
            "only-window" => {
                self.only_window();
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
            None => {
                for doc in &self.buffers {
                    if let Some(path) = doc.path() {
                        swap::clear(path);
                    }
                }
                Outcome::Quit
            }
        }
    }
}

/// The key that produces `motion`, so a repeat can go back through the same range calculation.
fn key_for_motion(motion: Motion) -> KeyCode {
    match motion {
        Motion::Left => KeyCode::Char('h'),
        Motion::Right => KeyCode::Char('l'),
        Motion::Up => KeyCode::Char('k'),
        Motion::Down => KeyCode::Char('j'),
        Motion::WordForward => KeyCode::Char('w'),
        Motion::WordBackward => KeyCode::Char('b'),
        Motion::WordEnd => KeyCode::Char('e'),
        Motion::WordEndBig => KeyCode::Char('E'),
        Motion::LineStart => KeyCode::Char('0'),
        Motion::LineEnd => KeyCode::Char('$'),
    }
}

/// Enclosing pair of `open`/`close` around `cursor` on one line, as indexes into `chars`.
fn enclosing_pair(
    chars: &[char],
    cursor: usize,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    if open == close {
        let mut index = 0;
        while index < chars.len() {
            if chars[index] == open {
                if let Some(offset) = chars[index + 1..].iter().position(|ch| *ch == close) {
                    let end = index + 1 + offset;
                    if index <= cursor && cursor <= end {
                        return Some((index, end));
                    }
                    index = end + 1;
                    continue;
                }
            }
            index += 1;
        }
        return None;
    }

    let mut depth = 0i32;
    let mut start = None;
    for index in (0..=cursor.min(chars.len().saturating_sub(1))).rev() {
        if chars[index] == close {
            depth += 1;
        } else if chars[index] == open {
            if depth == 0 {
                start = Some(index);
                break;
            }
            depth -= 1;
        }
    }
    let start = start?;

    let mut depth = 0i32;
    for (index, ch) in chars.iter().enumerate().skip(start + 1) {
        if *ch == open {
            depth += 1;
        } else if *ch == close {
            if depth == 0 {
                return Some((start, index));
            }
            depth -= 1;
        }
    }
    None
}

/// Byte offset of character column `col` in `text`.
fn byte_of_column(text: &str, col: usize) -> usize {
    text.char_indices()
        .nth(col)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
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
        assert_eq!(app.cursor(), Pos::new(0, 2));
        keys(&mut app, "j");
        assert_eq!(app.cursor(), Pos::new(1, 2));
        keys(&mut app, "h");
        assert_eq!(app.cursor(), Pos::new(1, 1));
        keys(&mut app, "k");
        assert_eq!(app.cursor(), Pos::new(0, 1));
    }

    #[test]
    fn counts_repeat_motions() {
        let mut app = app_with("abcdef\n");
        keys(&mut app, "4l");
        assert_eq!(app.cursor(), Pos::new(0, 4));
        keys(&mut app, "2h");
        assert_eq!(app.cursor(), Pos::new(0, 2));
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
        assert_eq!(app.cursor(), Pos::new(3, 0));
        keys(&mut app, "gg");
        assert_eq!(app.cursor(), Pos::new(0, 0));
        keys(&mut app, "3G");
        assert_eq!(app.cursor(), Pos::new(2, 0));
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
        let text = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = app_with(&text);
        app.follow_cursor(10);
        assert_eq!(app.window_snapshot(app.focused_window()).unwrap().scroll, 0);
        keys(&mut app, "30j");
        app.follow_cursor(10);
        assert!(app.window_snapshot(app.focused_window()).unwrap().scroll > 0);
        assert!(app.cursor().line < app.window_snapshot(app.focused_window()).unwrap().scroll + 10);
        assert!(app.cursor().line >= app.window_snapshot(app.focused_window()).unwrap().scroll);
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
        app.handle_key(ctrl('b'));
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
        assert_eq!(app.cursor(), Pos::new(1, 4));
        assert_eq!(app.search_progress(), Some((1, 2, false)));

        enter(&mut app);
        assert_eq!(app.mode(), Mode::Normal);
        assert!(app.search_highlight().is_some());

        keys(&mut app, "n");
        assert_eq!(app.cursor(), Pos::new(2, 0));
        assert_eq!(app.search_progress().unwrap().0, 2);
        keys(&mut app, "N");
        assert_eq!(app.cursor(), Pos::new(1, 4));
    }

    #[test]
    fn escape_cancels_search_and_restores_the_cursor() {
        let mut app = app_with("one\nbeta\n");
        search(&mut app, "beta");
        assert_eq!(app.cursor(), Pos::new(1, 0));
        esc(&mut app);
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.cursor(), Pos::new(0, 0));
        assert!(app.search_highlight().is_none());
        assert!(app.search_progress().is_none());
    }

    #[test]
    fn search_wraps_and_reports_when_nothing_matches() {
        let mut app = app_with("beta\nbeta\n");
        search(&mut app, "beta");
        enter(&mut app);
        keys(&mut app, "n");
        assert_eq!(app.cursor(), Pos::new(1, 0));
        keys(&mut app, "n");
        assert_eq!(app.cursor(), Pos::new(0, 0), "wraps around");

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

    #[test]
    fn substitute_on_the_current_line_replaces_the_first_match() {
        let mut app = app_with("foo foo\nfoo\n");
        run_cmd(&mut app, "%s/foo/bar/");
        assert_eq!(app.document().line(0), "bar foo");
        assert_eq!(app.document().line(1), "bar");
        assert!(app.message().unwrap().contains("replaced 2"));

        keys(&mut app, "u");
        assert_eq!(
            app.document().line(0),
            "foo foo",
            "one undo step for the whole substitute"
        );
    }

    #[test]
    fn global_substitute_replaces_every_match_on_every_line() {
        let mut app = app_with("foo foo\nfoo\n");
        run_cmd(&mut app, "%s/foo/bar/g");
        assert_eq!(app.document().text(), "bar bar\nbar\n");
    }

    #[test]
    fn substitute_without_percent_only_touches_the_current_line() {
        let mut app = app_with("foo\nfoo\n");
        run_cmd(&mut app, "s/foo/bar/");
        assert_eq!(app.document().text(), "bar\nfoo\n");
    }

    #[test]
    fn substitute_reports_a_missing_pattern() {
        let mut app = app_with("alpha\n");
        run_cmd(&mut app, "%s/zzz/x/g");
        assert!(app.message().unwrap().contains("pattern not found"));
        assert_eq!(app.document().text(), "alpha\n");
    }

    #[test]
    fn confirm_substitute_asks_about_each_match() {
        let mut app = app_with("a b a b\n");
        run_cmd(&mut app, "%s/a/X/gc");
        assert_eq!(app.mode(), Mode::ReplaceConfirm);
        assert!(app.replace_prompt().unwrap().contains("1/2"));

        keys(&mut app, "y");
        assert!(app.replace_prompt().unwrap().contains("2/2"));
        keys(&mut app, "n");

        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.document().text(), "X b a b\n");
        assert!(app.message().unwrap().contains("replaced 1 of 2"));
    }

    #[test]
    fn confirm_substitute_can_accept_the_rest_with_a() {
        let mut app = app_with("a a a\n");
        run_cmd(&mut app, "%s/a/Z/gc");
        keys(&mut app, "n");
        keys(&mut app, "a");
        assert_eq!(app.document().text(), "a Z Z\n");
        assert!(app.message().unwrap().contains("replaced 2 of 3"));
    }

    #[test]
    fn confirm_substitute_can_be_abandoned_with_q() {
        let mut app = app_with("a a\n");
        run_cmd(&mut app, "%s/a/Q/gc");
        keys(&mut app, "y");
        keys(&mut app, "q");
        assert_eq!(app.document().text(), "Q a\n");
        assert_eq!(app.mode(), Mode::Normal);
    }

    #[test]
    fn vsplit_opens_a_second_window_on_the_same_buffer() {
        let mut app = app_with("alpha\nbeta\n");
        app.set_area(80, 24);
        run_cmd(&mut app, "vsp");
        assert_eq!(app.window_rects().len(), 2);
        assert_eq!(
            app.buffers().len(),
            1,
            "a split does not open a second file"
        );

        let right = app.focused_window();
        keys(&mut app, "j");
        assert_eq!(app.cursor().line, 1);
        let left = app.window_rects()[0].0;
        assert_eq!(
            app.window_snapshot(left).unwrap().cursor.line,
            0,
            "the other window keeps its own cursor"
        );

        app.focus_direction('h');
        assert_eq!(app.focused_window(), left);
        assert_eq!(app.cursor().line, 0);
        let _ = right;
    }

    #[test]
    fn sp_stacks_windows_and_focus_moves_down() {
        let mut app = app_with("alpha\nbeta\n");
        app.set_area(80, 24);
        run_cmd(&mut app, "sp");
        let [top, bottom] = [app.window_rects()[0].0, app.window_rects()[1].0];
        assert_eq!(app.focused_window(), bottom);
        app.focus_direction('k');
        assert_eq!(app.focused_window(), top);
        app.focus_direction('j');
        assert_eq!(app.focused_window(), bottom);
    }

    #[test]
    fn closing_a_window_keeps_the_file_open() {
        let mut app = app_with("alpha\n");
        run_cmd(&mut app, "vsp");
        assert_eq!(app.window_rects().len(), 2);
        run_cmd(&mut app, "close");
        assert_eq!(app.window_rects().len(), 1);
        assert_eq!(app.buffers().len(), 1);
    }

    #[test]
    fn the_last_window_cannot_be_closed() {
        let mut app = app_with("alpha\n");
        run_cmd(&mut app, "close");
        assert_eq!(app.window_rects().len(), 1);
        assert!(app.message().unwrap().contains("last window"));
    }

    #[test]
    fn only_closes_every_other_window() {
        let mut app = app_with("alpha\n");
        run_cmd(&mut app, "vsp");
        run_cmd(&mut app, "sp");
        assert_eq!(app.window_rects().len(), 3);
        run_cmd(&mut app, "only");
        assert_eq!(app.window_rects().len(), 1);
    }

    #[test]
    fn asking_for_a_window_that_is_not_there_says_so() {
        let mut app = app_with("alpha\n");
        app.set_area(80, 24);
        app.focus_direction('l');
        assert!(app.message().unwrap().contains("no window to the l"));
    }

    #[test]
    fn ctrl_w_opens_the_window_command_prompt() {
        let mut app = app_with("alpha\n");
        app.handle_key(ctrl('w'));
        assert_eq!(app.mode(), Mode::WindowCmd);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode(), Mode::Normal);
    }

    #[test]
    fn closing_a_buffer_that_a_second_window_shows_falls_back() {
        let (mut app, path) = app_with_path("alpha\n");
        let other = path.with_file_name("other-split.txt");
        fs::write(&other, "other\n").unwrap();
        run_cmd(&mut app, &format!("e {}", other.display()));
        run_cmd(&mut app, "vsp");

        // the focused window shows the second file; close it
        run_cmd(&mut app, "bd");
        assert_eq!(app.buffers().len(), 1);
        assert_eq!(app.window_rects().len(), 2);
        assert_eq!(app.document().display_name(), "buffer.txt");
    }

    /// A throwaway git repository with one committed file.
    fn repo_with_commit(name: &str, contents: &str) -> PathBuf {
        use std::process::Command;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let dir = std::env::temp_dir().join(format!(
            "nyavim-git-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .expect("git must be installed for the test suite");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "initial"]);
        path
    }

    #[test]
    fn git_marks_compare_the_buffer_with_head() {
        let path = repo_with_commit("tracked.txt", "one\ntwo\n");
        let mut app = App::new();
        app.open(&path);

        assert_eq!(app.git_info().branch.as_deref(), Some("main"));
        assert!(app
            .git_info()
            .marks
            .iter()
            .all(|mark| *mark == crate::git::Mark::Unchanged));

        keys(&mut app, "A changed");
        esc(&mut app);
        assert_eq!(app.git_info().marks[0], crate::git::Mark::Modified);
    }

    #[test]
    fn a_file_that_is_not_in_head_is_all_added() {
        let path = repo_with_commit("tracked.txt", "one\n");
        let dir = path.parent().unwrap().to_path_buf();
        let fresh = dir.join("new.txt");
        fs::write(&fresh, "a\nb\n").unwrap();

        let mut app = App::new();
        app.open(&fresh);
        assert_eq!(app.git_info().marks, vec![crate::git::Mark::Added; 2]);
    }

    #[test]
    fn commit_stages_tracked_changes_and_clears_the_marks() {
        let path = repo_with_commit("tracked.txt", "one\n");
        let mut app = App::new();
        app.open(&path);
        keys(&mut app, "A-two");
        esc(&mut app);
        run_cmd(&mut app, "w");

        run_cmd(&mut app, "commit");
        assert_eq!(app.mode(), Mode::CommitMsg);
        for ch in "add two".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        enter(&mut app);

        assert_eq!(app.mode(), Mode::Normal);
        let message = app.message().unwrap().to_owned();
        assert!(!message.contains("failed"), "commit failed: {message}");
        assert!(
            app.git_info()
                .marks
                .iter()
                .all(|mark| *mark == crate::git::Mark::Unchanged),
            "after committing there is nothing left to mark"
        );
    }

    #[test]
    fn commit_refuses_an_unsaved_buffer() {
        let path = repo_with_commit("tracked.txt", "one\n");
        let mut app = App::new();
        app.open(&path);
        keys(&mut app, "A-two");
        esc(&mut app);
        run_cmd(&mut app, "commit");
        assert_eq!(app.mode(), Mode::Normal);
        assert!(app.message().unwrap().contains("save the file first"));
    }

    #[test]
    fn a_large_file_opens_read_only_and_refuses_edits() {
        let dir = std::env::temp_dir().join(format!("nyavim-ro-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.txt");
        fs::write(&path, "one two three four five\n").unwrap();

        let mut doc = crate::document::Document::open_with_limit(&path, 4).unwrap();
        assert!(doc.is_read_only());
        let mut cursor = Pos::new(0, 0);
        doc.insert_char(&mut cursor, 'x');
        assert_eq!(
            doc.text(),
            "one two three four five\n",
            "nothing was inserted"
        );
        assert!(!doc.is_dirty());

        let writable = crate::document::Document::open_with_limit(&path, 1024).unwrap();
        assert!(!writable.is_read_only());
    }

    #[test]
    fn clicking_a_tab_switches_buffer_and_clicking_text_moves_the_cursor() {
        let (mut app, path) = app_with_path("first\nsecond\n");
        let other = path.with_file_name("clicked.txt");
        fs::write(&other, "other\n").unwrap();
        run_cmd(&mut app, &format!("e {}", other.display()));
        app.set_area(60, 10);

        // the first tab starts at column 0, so clicking 1 selects it
        app.mouse_click(1, 0);
        assert_eq!(app.active(), 0);
        assert_eq!(app.document().display_name(), "buffer.txt");

        // body starts on row 1; the gutter is 4 cells (3 digits + space)
        app.mouse_click(4 + 2, 2);
        assert_eq!(app.cursor(), Pos::new(1, 2));
    }

    #[test]
    fn the_wheel_scrolls_the_window_under_the_pointer() {
        let text = (0..50)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = app_with(&text);
        app.set_area(60, 10);
        app.follow_cursor(7);
        app.mouse_scroll(5, 2, 3);
        assert_eq!(app.window_snapshot(app.focused_window()).unwrap().scroll, 3);
        app.mouse_scroll(5, 2, -10);
        assert_eq!(app.window_snapshot(app.focused_window()).unwrap().scroll, 0);
    }

    #[test]
    fn a_swap_file_offers_recovery_when_the_file_is_opened() {
        let (_, path) = app_with_path("on disk\n");
        crate::swap::write(&path, "recovered text\n").unwrap();

        let mut reopened = App::new();
        reopened.open(&path);
        assert!(reopened.swap_prompt().is_some());
        assert_eq!(
            reopened.document().text(),
            "on disk\n",
            "nothing applied yet"
        );

        keys(&mut reopened, "r");
        assert_eq!(reopened.document().text(), "recovered text\n");
        assert!(reopened.document().is_dirty());
        assert!(
            crate::swap::read(&path).is_none(),
            "the swap file is consumed"
        );
    }

    #[test]
    fn discarding_a_swap_file_keeps_the_disk_version() {
        let (_, path) = app_with_path("on disk\n");
        crate::swap::write(&path, "ignored\n").unwrap();

        let mut app = App::new();
        app.open(&path);
        keys(&mut app, "d");
        assert_eq!(app.document().text(), "on disk\n");
        assert!(!app.document().is_dirty());
        assert!(crate::swap::read(&path).is_none());
    }

    #[test]
    fn saving_a_dirty_buffer_clears_its_swap_file() {
        let (mut app, path) = app_with_path("start\n");
        keys(&mut app, "A!x");
        esc(&mut app);
        app.tick();
        assert!(
            crate::swap::read(&path).is_some(),
            "a dirty buffer is mirrored"
        );

        run_cmd(&mut app, "w");
        assert!(
            crate::swap::read(&path).is_none(),
            "saving removes the swap file"
        );
    }

    #[test]
    fn configuration_changes_tab_width_and_theme() {
        let dir = std::env::temp_dir().join(format!("nyavim-cfgapp-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        fs::write(
            &cfg,
            "[editor]\ntab_width = 8\n[theme]\nkeyword = \"red\"\n",
        )
        .unwrap();

        let mut app = App::new();
        app.set_config(crate::config::Config::load_from(&cfg));
        assert_eq!(app.config().tab_width, 8);
        assert_eq!(app.config().theme.keyword, ratatui::style::Color::Red);
        assert!(app.config().path.is_some());
    }

    #[test]
    fn a_config_warning_is_reported_once() {
        let dir = std::env::temp_dir().join(format!("nyavim-cfgwarn-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        fs::write(&cfg, "[theme]\nkeyword = \"chartreuse\"\n").unwrap();

        let mut app = App::new();
        app.set_config(crate::config::Config::load_from(&cfg));
        assert!(app.message().unwrap().contains("config:"));
    }

    #[test]
    fn filetype_overrides_drive_highlighting_language() {
        let dir = std::env::temp_dir().join(format!("nyavim-cfgft-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        fs::write(&cfg, "[filetypes]\npy = \"rust\"\n").unwrap();
        let file = dir.join("script.py");
        fs::write(&file, "let x = 1;\n").unwrap();

        let mut app = App::new();
        app.set_config(crate::config::Config::load_from(&cfg));
        app.open(&file);
        assert_eq!(app.language(), crate::syntax::Language::Rust);
    }

    #[test]
    fn visual_mode_selects_and_deletes_a_range() {
        let mut app = app_with("alpha beta gamma\n");
        keys(&mut app, "wv"); // start visual at "beta"
        assert_eq!(app.mode(), Mode::Visual);
        assert_eq!(app.selection(), Some((Pos::new(0, 6), Pos::new(0, 7))));
        keys(&mut app, "l l"); // extend two characters
        keys(&mut app, "d");
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.document().line(0), "alpha a gamma");
        assert_eq!(app.selection(), None);
    }

    #[test]
    fn visual_yank_then_put_uses_the_unnamed_register() {
        let mut app = app_with("one two\n");
        keys(&mut app, "vll"); // select "one"
        keys(&mut app, "y");
        assert_eq!(app.mode(), Mode::Normal);
        keys(&mut app, "$");
        keys(&mut app, "p");
        assert_eq!(app.document().line(0), "one twoone");
    }

    #[test]
    fn visual_escape_cancels_without_touching_text() {
        let mut app = app_with("hello\n");
        keys(&mut app, "vll");
        esc(&mut app);
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.document().line(0), "hello");
    }

    #[test]
    fn diw_deletes_the_word_under_the_cursor() {
        let mut app = app_with("alpha beta gamma\n");
        keys(&mut app, "w"); // on "beta"
        keys(&mut app, "diw");
        assert_eq!(app.document().line(0), "alpha  gamma");
    }

    #[test]
    fn daw_takes_the_trailing_space_too() {
        let mut app = app_with("alpha beta gamma\n");
        keys(&mut app, "w");
        keys(&mut app, "daw");
        assert_eq!(app.document().line(0), "alpha gamma");
    }

    #[test]
    fn a_quote_text_object_selects_inside_and_around() {
        let mut app = app_with("say \"hello world\" now\n");
        keys(&mut app, "f"); // no-op, keeps us at the start
        for _ in 0..6 {
            keys(&mut app, "l"); // inside the quotes
        }
        keys(&mut app, "ci");
        keys(&mut app, "\"");
        keys(&mut app, "bye");
        esc(&mut app);
        assert_eq!(app.document().line(0), "say \"bye\" now");
    }

    #[test]
    fn percent_jumps_between_brackets() {
        let mut app = app_with("fn main() {\n  body();\n}\n");
        for _ in 0..10 {
            keys(&mut app, "l");
        }
        assert_eq!(app.document().char_at_pos(app.cursor()), Some('{'));
        keys(&mut app, "%");
        assert_eq!(
            app.cursor().line,
            2,
            "the closing brace is on the last line"
        );
        keys(&mut app, "%");
        assert_eq!(app.cursor(), Pos::new(0, 10), "and back again");
    }

    #[test]
    fn a_named_register_keeps_its_own_yank() {
        let mut app = app_with("keep\nother\n");
        keys(&mut app, "\"ayy"); // yank line 1 into register a
        keys(&mut app, "j");
        keys(&mut app, "\"ap"); // paste it below line 2
        assert_eq!(app.document().line(2), "keep");
    }

    #[test]
    fn shift_j_joins_the_next_line() {
        let mut app = app_with("first\nsecond\n");
        keys(&mut app, "J");
        assert_eq!(app.document().text(), "first second\n");
    }

    #[test]
    fn question_mark_searches_backwards_and_n_keeps_going_back() {
        let mut app = app_with("beta\nbeta\nbeta\n");
        keys(&mut app, "G"); // last line
        assert_eq!(app.cursor().line, 2);
        keys(&mut app, "?beta");
        assert_eq!(app.cursor().line, 1, "the search found the match above");
        enter(&mut app);
        keys(&mut app, "n");
        assert_eq!(app.cursor().line, 0, "n continues backwards");
        keys(&mut app, "N");
        assert_eq!(app.cursor().line, 1, "N reverses direction");
        assert_eq!(app.search_prefix(), "?");
    }

    #[test]
    fn ctrl_p_lists_project_files_and_opens_the_choice() {
        let dir = std::env::temp_dir().join(format!("nyavim-picker-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.join("notes.txt"), "notes\n").unwrap();
        let start = dir.join("notes.txt");

        let mut app = App::new();
        app.set_area(80, 24);
        app.open(&start);
        app.handle_key(ctrl('p'));
        assert_eq!(app.mode(), Mode::FindFile);
        assert!(app
            .finder_matches()
            .iter()
            .any(|file| file.ends_with("main.rs")));

        for ch in "main".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(app.finder_matches()[0], "src/main.rs");
        enter(&mut app);
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.document().line(0), "fn main() {}");
    }

    #[test]
    fn escape_closes_the_picker_without_opening_anything() {
        let (mut app, path) = app_with_path("keep\n");
        app.open(&path);
        app.handle_key(ctrl('p'));
        esc(&mut app);
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.buffers().len(), 1);
    }

    #[test]
    fn dot_repeats_the_last_operator_with_its_motion() {
        let mut app = app_with("one two three four\n");
        keys(&mut app, "dw"); // delete "one "
        assert_eq!(app.document().line(0), "two three four");
        keys(&mut app, ".");
        assert_eq!(app.document().line(0), "three four");
        keys(&mut app, ".");
        assert_eq!(app.document().line(0), "four");
    }

    #[test]
    fn dot_repeats_a_delete_character_and_a_paste() {
        let mut app = app_with("abc\n");
        keys(&mut app, "x");
        keys(&mut app, ".");
        assert_eq!(app.document().line(0), "c");

        keys(&mut app, "p"); // put "b" back
        keys(&mut app, ".");
        assert_eq!(app.document().line(0), "cbb");
    }

    #[test]
    fn dot_replays_the_text_of_an_insert_session() {
        let mut app = app_with("a\nb\n");
        keys(&mut app, "A-ok");
        esc(&mut app);
        assert_eq!(app.document().line(0), "a-ok");
        keys(&mut app, "j");
        keys(&mut app, ".");
        assert_eq!(app.document().line(1), "b-ok");
    }

    #[test]
    fn dot_without_a_previous_change_says_so() {
        let mut app = app_with("abc\n");
        keys(&mut app, ".");
        assert!(app.message().unwrap().contains("nothing to repeat"));
    }

    #[test]
    fn e_and_big_e_land_on_word_ends() {
        let mut app = app_with("alpha beta_gamma delta\n");
        keys(&mut app, "e");
        assert_eq!(app.cursor(), Pos::new(0, 4), "end of the first word");
        keys(&mut app, "e");
        assert_eq!(app.cursor(), Pos::new(0, 15), "end of the second word");
        keys(&mut app, "0");
        keys(&mut app, "E");
        assert_eq!(
            app.cursor(),
            Pos::new(0, 4),
            "E stops at the same first word end"
        );
    }

    #[test]
    fn e_is_inclusive_for_operators() {
        let mut app = app_with("alpha beta\n");
        keys(&mut app, "de");
        assert_eq!(
            app.document().line(0),
            " beta",
            "de deletes through the last character"
        );
    }

    #[test]
    fn dot_repeats_an_operator_with_the_word_end_motion() {
        let mut app = app_with("one two three\n");
        keys(&mut app, "de");
        assert_eq!(app.document().line(0), " two three");
        keys(&mut app, ".");
        assert_eq!(app.document().line(0), " three");
    }

    #[test]
    fn dot_repeats_a_visual_selection_by_width() {
        let mut app = app_with("abcdefghi\n");
        keys(&mut app, "vlld"); // select "abc" and delete it
        assert_eq!(app.document().line(0), "defghi");
        keys(&mut app, ".");
        assert_eq!(app.document().line(0), "ghi", "three more characters go");
    }

    #[test]
    fn resize_moves_the_divider_of_the_focused_window() {
        let mut app = app_with("alpha\n");
        app.set_area(80, 24);
        run_cmd(&mut app, "vsp");
        let before: Vec<u16> = app
            .window_rects()
            .iter()
            .map(|(_, rect)| rect.width)
            .collect();
        run_cmd(&mut app, "resize +10");
        let after: Vec<u16> = app
            .window_rects()
            .iter()
            .map(|(_, rect)| rect.width)
            .collect();
        assert_ne!(before, after, "the split moved");
        assert!(app.message().unwrap().contains("resized"));
    }

    #[test]
    fn resize_with_one_window_is_refused() {
        let mut app = app_with("alpha\n");
        run_cmd(&mut app, "resize +10");
        assert!(app.message().unwrap().contains("only one window"));
    }
}
