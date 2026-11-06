//! Application state and key routing. Rendering lives in [`crate::ui`]; text and files live in
//! [`crate::document`]. Nothing here touches the terminal, so the whole thing is testable without a
//! tty.

use std::path::Path;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::command::{self, Command, Substitute};
use crate::config::Config;
use crate::document::{is_case_sensitive, ordered, Document, LoadError, Pos};
use crate::git::{self, GitInfo};
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
    /// The shell session while the terminal owns the screen.
    terminal: Option<Session>,
    /// Commit message being typed after `:commit`.
    commit_message: String,
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
            insert_started: false,
            search: None,
            replacer: None,
            config: Config::default(),
            swap_prompt: None,
            last_swap_write: None,
            terminal: None,
            commit_message: String::new(),
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
        self.config = config;
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
        let line = window.scroll + (y - rect.y) as usize;
        if line >= self.document().line_count() {
            return;
        }
        let column = self.column_at_screen_x(line, rect.x, x);
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
                KeyCode::Char('w') => {
                    self.mode = Mode::WindowCmd;
                    Outcome::Continue
                }
                KeyCode::Char('t') => {
                    self.open_terminal();
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
        let cursor = self.cursor();
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
            KeyCode::Char('i') => self.enter_insert(),
            KeyCode::Char('a') => {
                self.move_cursor_right(1);
                self.enter_insert();
            }
            KeyCode::Char('I') => {
                self.set_cursor(Pos::new(cursor.line, 0));
                self.enter_insert();
            }
            KeyCode::Char('A') => {
                let end = self.document().line_len(cursor.line);
                self.set_cursor(Pos::new(cursor.line, end));
                self.enter_insert();
            }
            KeyCode::Char('o') => {
                self.checkpoint();
                self.insert_started = true;
                let end = self.document().line_len(cursor.line);
                self.set_cursor(Pos::new(cursor.line, end));
                self.edit(|doc, c| doc.insert_newline(c));
                self.enter_insert();
            }
            KeyCode::Char('O') => {
                self.checkpoint();
                self.insert_started = true;
                self.set_cursor(Pos::new(cursor.line, 0));
                self.edit(|doc, c| doc.insert_newline(c));
                self.set_cursor(Pos::new(cursor.line, 0));
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
        }
    }

    fn apply_operator(&mut self, op: Operator, key: KeyEvent) {
        self.input.operator = None;
        if self.document().is_read_only() {
            self.message = Some("read-only: file is larger than 50 MB".to_owned());
            return;
        }
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
                self.set_cursor(start);
            }
            Operator::Delete => {
                self.doc_mut().remove(start, end);
                self.register = Some(Register {
                    text,
                    linewise: false,
                });
                self.set_cursor(start);
                self.clamp_cursor();
            }
            Operator::Change => {
                self.doc_mut().remove(start, end);
                self.register = Some(Register {
                    text,
                    linewise: false,
                });
                self.set_cursor(start);
                self.enter_insert();
            }
        }
    }

    /// The half-open range an operator would touch if followed by `code`, found by running the
    /// motion on a copy of the buffer.
    fn operator_range(&self, code: KeyCode, count: usize) -> Option<(Pos, Pos)> {
        let start = self.cursor();
        let doc = self.document();
        let (target, inclusive) = match code {
            KeyCode::Char('h') | KeyCode::Left => (doc.move_left(start, count), false),
            KeyCode::Char('l') | KeyCode::Right => (doc.move_right(start, count), false),
            KeyCode::Char('w') => (doc.move_word_forward(start, count), false),
            KeyCode::Char('b') => (doc.move_word_backward(start, count), false),
            KeyCode::Char('0') | KeyCode::Home => (Pos::new(start.line, 0), false),
            KeyCode::Char('$') | KeyCode::End => (doc.move_line_end(start), true),
            _ => return None,
        };
        let (a, b) = ordered(start, target);
        if inclusive {
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
                self.set_cursor(Pos::new(line, 0));
                self.register = Some(Register {
                    text,
                    linewise: true,
                });
                self.enter_insert();
            }
        }
    }

    fn paste(&mut self, before: bool) {
        if self.document().is_read_only() {
            self.message = Some("read-only: file is larger than 50 MB".to_owned());
            return;
        }
        let Some(register) = self.register.clone() else {
            self.message = Some("nothing to paste yet".to_owned());
            return;
        };
        self.checkpoint();
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

    fn start_search(&mut self) {
        self.search = Some(Search {
            query: String::new(),
            origin: self.cursor(),
            origin_scroll: self.windows[self.focus].scroll,
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
        let from = self.next_position(self.cursor());
        let found = self
            .document()
            .find_forward(from, &query, case_sensitive)
            .or_else(|| {
                self.document()
                    .find_forward(Pos::new(0, 0), &query, case_sensitive)
            });
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
        let cursor = self.cursor();
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
}
