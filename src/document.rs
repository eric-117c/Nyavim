//! The text buffer and its file on disk.
//!
//! The rope always stores text with `\n` line endings; the file's original ending and BOM are
//! remembered separately and restored on save, so editing a CRLF file never silently rewrites every
//! line of it.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ropey::Rope;

const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    /// Detect the ending of a file's contents. Mixed files are treated as CRLF, since writing them
    /// back as CRLF is what every editor that does not rewrite the file does.
    fn detect(text: &str) -> Self {
        if text.contains("\r\n") {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        }
    }

    fn apply(self, text: &str) -> String {
        match self {
            LineEnding::Lf => text.to_owned(),
            LineEnding::Crlf => text.replace('\n', "\r\n"),
        }
    }
}

#[derive(Debug)]
pub enum LoadError {
    Io(io::Error),
    NotUtf8,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(err) => write!(f, "{err}"),
            LoadError::NotUtf8 => write!(
                f,
                "not a UTF-8 text file (only UTF-8 is supported in this version)"
            ),
        }
    }
}

impl From<io::Error> for LoadError {
    fn from(err: io::Error) -> Self {
        LoadError::Io(err)
    }
}

/// A (line, column) position. Columns count characters, not bytes or display cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

#[derive(Debug, Clone)]
pub struct Document {
    rope: Rope,
    path: Option<PathBuf>,
    line_ending: LineEnding,
    has_bom: bool,
    /// Bumped by every mutation. Compared against `saved_revision` to answer "is this buffer
    /// dirty?", which makes undo back to the saved state correctly clean again.
    revision: u64,
    saved_revision: u64,
    /// Undo history: whole-buffer snapshots, newest last. Cheap to reason about and good enough
    /// because normal mode gathers a change per command rather than per keystroke.
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    read_only: bool,
}

#[derive(Debug, Clone)]
struct Snapshot {
    text: String,
    cursor: Pos,
    revision: u64,
}

/// Oldest undo steps are dropped past these limits, so a big file cannot grow memory without bound.
const UNDO_STEPS: usize = 200;
const UNDO_BUDGET_BYTES: usize = 8 * 1024 * 1024;

impl Document {
    /// An unnamed, empty buffer.
    pub fn empty() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            line_ending: LineEnding::Lf,
            has_bom: false,
            revision: 0,
            saved_revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            read_only: false,
        }
    }

    pub fn from_text(text: &str) -> Self {
        let mut doc = Self::empty();
        doc.line_ending = LineEnding::detect(text);
        doc.rope = Rope::from_str(&text.replace("\r\n", "\n"));
        doc
    }

    /// An empty buffer that will be written to `path` on the first save.
    pub fn new_at(path: impl AsRef<Path>) -> Self {
        let mut doc = Self::empty();
        doc.path = Some(path.as_ref().to_path_buf());
        doc
    }

    /// Open a file, read-only when it is larger than `limit`. The limit comes from the user's
    /// configuration, so this module never decides policy on its own.
    pub fn open_with_limit(path: impl AsRef<Path>, limit: u64) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let bytes = fs::read(path)?;
        let (has_bom, body) = match bytes.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, &bytes[..]),
        };
        let text = std::str::from_utf8(body).map_err(|_| LoadError::NotUtf8)?;

        let mut doc = Self::from_text(text);
        doc.has_bom = has_bom;
        doc.path = Some(path.to_path_buf());
        doc.read_only = (bytes.len() as u64) > limit;
        Ok(doc)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Name shown in the tab bar and status line.
    pub fn display_name(&self) -> String {
        match &self.path {
            Some(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            None => "[No Name]".to_owned(),
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    /// Remember the current state as the point `undo` returns to. Call this once before a change
    /// (one insert session, one operator), not per character.
    pub fn checkpoint(&mut self, cursor: Pos) {
        self.undo.push(self.snapshot(cursor));
        self.redo.clear();
        if self.undo.len() > UNDO_STEPS {
            self.undo.remove(0);
        }
        while self.undo.len() > 1
            && self.undo.iter().map(|s| s.text.len()).sum::<usize>() > UNDO_BUDGET_BYTES
        {
            self.undo.remove(0);
        }
    }

    /// Step back to the last checkpoint, returning the cursor that went with it.
    pub fn undo(&mut self, cursor: Pos) -> Option<Pos> {
        let previous = self.undo.pop()?;
        self.redo.push(self.snapshot(cursor));
        Some(self.apply(previous))
    }

    /// Step forward again, returning the cursor that went with it.
    pub fn redo(&mut self, cursor: Pos) -> Option<Pos> {
        let next = self.redo.pop()?;
        self.undo.push(self.snapshot(cursor));
        Some(self.apply(next))
    }

    fn snapshot(&self, cursor: Pos) -> Snapshot {
        Snapshot {
            text: self.rope.to_string(),
            cursor,
            revision: self.revision,
        }
    }

    fn apply(&mut self, snapshot: Snapshot) -> Pos {
        self.rope = Rope::from_str(&snapshot.text);
        self.revision = snapshot.revision;
        self.clamp_pos(snapshot.cursor)
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn has_bom(&self) -> bool {
        self.has_bom
    }

    /// Number of lines. A trailing newline does not create an extra visible line.
    pub fn line_count(&self) -> usize {
        let len_lines = self.rope.len_lines();
        if len_lines > 1 && self.rope.line(len_lines - 1).len_chars() == 0 {
            len_lines - 1
        } else {
            len_lines
        }
    }

    /// One line without its line ending.
    pub fn line(&self, idx: usize) -> String {
        if idx >= self.rope.len_lines() {
            return String::new();
        }
        let mut text = self.rope.line(idx).to_string();
        while text.ends_with('\n') || text.ends_with('\r') {
            text.pop();
        }
        text
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    // ---- cursor and motions -------------------------------------------

    /// Longest valid column, i.e. one past the last character (insert-mode position).
    pub fn line_len(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() {
            return 0;
        }
        let seg = self.rope.line(line);
        let chars = seg.len_chars();
        if chars > 0 && seg.char(chars - 1) == '\n' {
            chars - 1
        } else {
            chars
        }
    }

    /// Clamp a position into the buffer.
    pub fn clamp_pos(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.line_count().saturating_sub(1));
        let col = pos.col.min(self.line_len(line));
        Pos { line, col }
    }

    /// Keep the cursor on a character when in normal mode, where sitting past the last character
    /// would show a cursor that cannot be acted on.
    pub fn clamp_normal_pos(&self, pos: Pos) -> Pos {
        let pos = self.clamp_pos(pos);
        let len = self.line_len(pos.line);
        Pos::new(pos.line, if len == 0 { 0 } else { pos.col.min(len - 1) })
    }

    fn char_index(&self, pos: Pos) -> usize {
        self.rope.line_to_char(pos.line) + pos.col
    }

    // ---- primitive text operations ------------------------------------

    /// Text between two positions, ordered low to high.
    pub fn slice(&self, a: Pos, b: Pos) -> String {
        let (a, b) = ordered(self.clamp_pos(a), self.clamp_pos(b));
        self.rope
            .slice(self.char_index(a)..self.char_index(b))
            .to_string()
    }

    /// Remove and return the text between two positions. The cursor is the caller's business.
    pub fn remove(&mut self, a: Pos, b: Pos) -> String {
        if self.read_only {
            return String::new();
        }
        let (a, b) = ordered(self.clamp_pos(a), self.clamp_pos(b));
        let (from, to) = (self.char_index(a), self.char_index(b));
        if from >= to {
            return String::new();
        }
        let removed = self.rope.slice(from..to).to_string();
        self.rope.remove(from..to);
        self.revision += 1;
        removed
    }

    /// Insert text at a position without touching the cursor.
    pub fn insert_at(&mut self, pos: Pos, text: &str) {
        if text.is_empty() || self.read_only {
            return;
        }
        let at = self.clamp_pos(pos);
        let idx = self.char_index(at);
        self.rope.insert(idx, text);
        self.revision += 1;
    }

    pub fn insert_char(&mut self, cursor: &mut Pos, ch: char) {
        if self.read_only {
            return;
        }
        let idx = self.char_index(*cursor);
        self.rope.insert_char(idx, ch);
        self.revision += 1;
        cursor.col += 1;
    }

    pub fn insert_newline(&mut self, cursor: &mut Pos) {
        self.insert_char(cursor, '\n');
        cursor.line += 1;
        cursor.col = 0;
    }

    /// Delete the character before the cursor, joining lines when at the start of one.
    pub fn backspace(&mut self, cursor: &mut Pos) {
        if cursor.col > 0 {
            let from = Pos::new(cursor.line, cursor.col - 1);
            self.remove(from, *cursor);
            cursor.col -= 1;
        } else if cursor.line > 0 {
            let prev = cursor.line - 1;
            let join = Pos::new(prev, self.line_len(prev));
            self.remove(join, *cursor);
            *cursor = join;
        }
    }

    /// Delete the character under the cursor, joining lines when at the end of one.
    pub fn delete_forward(&mut self, cursor: &mut Pos) {
        let len = self.line_len(cursor.line);
        let to = if cursor.col < len {
            Pos::new(cursor.line, cursor.col + 1)
        } else if cursor.line + 1 < self.line_count() {
            Pos::new(cursor.line + 1, 0)
        } else {
            return;
        };
        self.remove(*cursor, to);
        *cursor = self.clamp_normal_pos(*cursor);
    }

    /// Remove a whole line including one of its line endings; returns the removed text.
    ///
    /// Works on raw rope lines so that deleting the last visible line of a file that ends with a
    /// newline removes that newline too instead of leaving a stray empty line behind.
    pub fn delete_line(&mut self, line: usize) -> String {
        if self.read_only || line >= self.line_count() {
            return String::new();
        }
        let mut start = self.rope.line_to_char(line);
        let raw_lines = self.rope.len_lines();
        let end = if line + 1 < raw_lines {
            self.rope.line_to_char(line + 1)
        } else if start > 0 {
            start -= 1; // pull in the newline that ends the previous line
            self.rope.len_chars()
        } else {
            self.rope.len_chars()
        };
        if start >= end {
            return String::new();
        }
        let removed = self.rope.slice(start..end).to_string();
        self.rope.remove(start..end);
        self.revision += 1;
        removed
    }

    /// Insert lines starting at `line`, as a linewise paste does. A `line` past the last line
    /// appends at the end of the buffer.
    pub fn insert_lines(&mut self, line: usize, lines: &[String]) {
        if lines.is_empty() || self.read_only {
            return;
        }
        let mut text = String::new();
        if line >= self.line_count() {
            if !self.text().ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&lines.join("\n"));
            text.push('\n');
            let end = self.rope.len_chars();
            self.rope.insert(end, &text);
            self.revision += 1;
        } else {
            text.push_str(&lines.join("\n"));
            text.push('\n');
            self.insert_at(Pos::new(line, 0), &text);
        }
    }

    // ---- motions (pure: they compute a new cursor) ---------------------

    /// `h`: never leaves the line, like vim.
    pub fn move_left(&self, cursor: Pos, count: usize) -> Pos {
        Pos::new(cursor.line, cursor.col.saturating_sub(count))
    }

    /// `l`.
    pub fn move_right(&self, cursor: Pos, count: usize) -> Pos {
        let col = (cursor.col + count).min(self.line_len(cursor.line));
        self.clamp_normal_pos(Pos::new(cursor.line, col))
    }

    /// `j`.
    pub fn move_down(&self, cursor: Pos, count: usize) -> Pos {
        let line = (cursor.line + count).min(self.line_count().saturating_sub(1));
        self.clamp_normal_pos(Pos::new(line, cursor.col))
    }

    /// `k`.
    pub fn move_up(&self, cursor: Pos, count: usize) -> Pos {
        self.clamp_normal_pos(Pos::new(cursor.line.saturating_sub(count), cursor.col))
    }

    /// `$`.
    pub fn move_line_end(&self, cursor: Pos) -> Pos {
        Pos::new(cursor.line, self.line_len(cursor.line).saturating_sub(1))
    }

    /// `w`: start of the next word, crossing lines.
    pub fn move_word_forward(&self, cursor: Pos, count: usize) -> Pos {
        let mut pos = cursor;
        for _ in 0..count.max(1) {
            self.skip_space_forward(&mut pos);
            if self.at_end_of_text(pos) {
                break;
            }
            let class = char_class(self.char_at(pos).unwrap_or(' '));
            while let Some(ch) = self.char_at(pos) {
                if char_class(ch) != class {
                    break;
                }
                pos.col += 1;
            }
            self.skip_space_forward(&mut pos);
        }
        self.clamp_normal_pos(pos)
    }

    /// `e` / `E`: last character of the current word, or of the next one. `big` treats any run of
    /// non-space characters as one word, which is what `E` means in vim.
    pub fn move_word_end(&self, cursor: Pos, count: usize, big: bool) -> Pos {
        let class_of = |ch: char| {
            if big {
                if ch.is_whitespace() {
                    0
                } else {
                    1
                }
            } else {
                char_class(ch)
            }
        };
        let mut pos = cursor;
        for _ in 0..count.max(1) {
            // always make progress, then land on the end of whatever run we are in
            if !self.step_forward(&mut pos) {
                break;
            }
            self.skip_space_forward(&mut pos);
            if self.at_end_of_text(pos) {
                break;
            }
            let class = class_of(self.char_at(pos).unwrap_or(' '));
            while let Some(next) = self.char_after(pos) {
                if class_of(next) != class {
                    break;
                }
                pos = Pos::new(pos.line, pos.col + 1);
            }
        }
        self.clamp_normal_pos(pos)
    }

    /// `b`: start of the previous word, crossing lines.
    pub fn move_word_backward(&self, cursor: Pos, count: usize) -> Pos {
        let mut pos = cursor;
        for _ in 0..count.max(1) {
            if !self.step_back_one(&mut pos) {
                break;
            }
            self.skip_space_backward(&mut pos);
            let class = match self.char_at(pos) {
                Some(ch) => char_class(ch),
                None => continue,
            };
            while pos.col > 0 {
                match self.char_at(Pos::new(pos.line, pos.col - 1)) {
                    Some(prev) if char_class(prev) == class => pos.col -= 1,
                    _ => break,
                }
            }
        }
        self.clamp_normal_pos(pos)
    }

    // ---- searching ----------------------------------------------------

    /// First match at or after `from`, scanning forward without wrapping.
    ///
    /// Walks the rope with a sequential line iterator: `rope.line(i)` per line is ten times slower
    /// on a large file, which is the difference between a responsive search and a stall.
    pub fn find_forward(&self, from: Pos, needle: &str, case_sensitive: bool) -> Option<Pos> {
        if needle.is_empty() || from.line >= self.line_count() {
            return None;
        }
        let lowered = lower_if_needed(needle, case_sensitive);
        for (offset, slice) in self.rope.lines_at(from.line).enumerate() {
            let line = from.line + offset;
            if line >= self.line_count() {
                break;
            }
            let start = if offset == 0 { from.col } else { 0 };
            if let Some(col) = slice_matches(slice, needle, &lowered, case_sensitive)
                .into_iter()
                .find(|&col| col >= start)
            {
                return Some(Pos::new(line, col));
            }
        }
        None
    }

    /// Last match at or before `from`, scanning backward without wrapping.
    pub fn find_backward(&self, from: Pos, needle: &str, case_sensitive: bool) -> Option<Pos> {
        if needle.is_empty() {
            return None;
        }
        let lowered = lower_if_needed(needle, case_sensitive);
        let mut best = None;
        for (line, slice) in self.rope.lines().enumerate() {
            if line > from.line || line >= self.line_count() {
                break;
            }
            let boundary = if line == from.line {
                Some(from.col)
            } else {
                None
            };
            if let Some(&col) = slice_matches(slice, needle, &lowered, case_sensitive)
                .iter()
                .rev()
                .find(|&&col| boundary.is_none_or(|bound| col <= bound))
            {
                best = Some(Pos::new(line, col));
            }
        }
        best
    }

    /// Character columns where `needle` starts on `line`, left to right and non-overlapping.
    pub fn find_all_in_line(&self, line: usize, needle: &str, case_sensitive: bool) -> Vec<usize> {
        if needle.is_empty() || line >= self.line_count() {
            return Vec::new();
        }
        let lowered = lower_if_needed(needle, case_sensitive);
        slice_matches(self.rope.line(line), needle, &lowered, case_sensitive)
    }

    /// Every match in buffer order, starting at `from_line`, at most `max_lines` lines and `limit`
    /// matches. `true` means the scan stopped at a limit rather than at the end of the buffer.
    pub fn scan_matches(
        &self,
        needle: &str,
        case_sensitive: bool,
        from_line: usize,
        max_lines: usize,
        limit: usize,
    ) -> (Vec<Pos>, bool) {
        let mut found = Vec::new();
        if needle.is_empty() || limit == 0 || from_line >= self.line_count() {
            return (found, false);
        }
        let lowered = lower_if_needed(needle, case_sensitive);
        for (offset, slice) in self.rope.lines_at(from_line).enumerate() {
            let line = from_line + offset;
            if offset >= max_lines || line >= self.line_count() {
                break;
            }
            for col in slice_matches(slice, needle, &lowered, case_sensitive) {
                found.push(Pos::new(line, col));
                if found.len() >= limit {
                    return (found, true);
                }
            }
        }
        (found, false)
    }

    /// One character forward, crossing to the next line. False at the end of the buffer.
    fn step_forward(&self, cursor: &mut Pos) -> bool {
        if cursor.col + 1 < self.line_len(cursor.line) {
            cursor.col += 1;
            true
        } else if cursor.line + 1 < self.line_count() {
            cursor.line += 1;
            cursor.col = 0;
            true
        } else {
            false
        }
    }

    /// The character one column to the right, if any.
    fn char_after(&self, pos: Pos) -> Option<char> {
        self.char_at(Pos::new(pos.line, pos.col + 1))
    }

    /// Move one character back, crossing to the end of the previous line. False at the start of
    /// the buffer.
    fn step_back_one(&self, cursor: &mut Pos) -> bool {
        if cursor.col > 0 {
            cursor.col -= 1;
            true
        } else if cursor.line > 0 {
            cursor.line -= 1;
            cursor.col = self.line_len(cursor.line).saturating_sub(1);
            true
        } else {
            false
        }
    }

    fn at_end_of_text(&self, cursor: Pos) -> bool {
        cursor.line + 1 >= self.line_count() && cursor.col >= self.line_len(cursor.line)
    }

    /// Character at a position, for bracket matching and text objects.
    pub fn char_at_pos(&self, pos: Pos) -> Option<char> {
        self.char_at(pos)
    }

    fn char_at(&self, pos: Pos) -> Option<char> {
        if pos.line >= self.line_count() || pos.col >= self.line_len(pos.line) {
            return None;
        }
        Some(self.rope.line(pos.line).char(pos.col))
    }

    fn skip_space_forward(&self, cursor: &mut Pos) {
        loop {
            match self.char_at(*cursor) {
                Some(ch) if ch.is_whitespace() => cursor.col += 1,
                Some(_) => break,
                None => {
                    if cursor.line + 1 < self.line_count() {
                        cursor.line += 1;
                        cursor.col = 0;
                    } else {
                        cursor.col = self.line_len(cursor.line);
                        break;
                    }
                }
            }
        }
    }

    fn skip_space_backward(&self, cursor: &mut Pos) {
        loop {
            if cursor.col == 0 {
                if cursor.line == 0 {
                    break;
                }
                cursor.line -= 1;
                cursor.col = self.line_len(cursor.line);
                continue;
            }
            match self.char_at(Pos::new(cursor.line, cursor.col - 1)) {
                Some(ch) if ch.is_whitespace() => cursor.col -= 1,
                _ => break,
            }
        }
    }

    /// Mark the buffer as having unsaved changes without touching its text, for "recovered from a
    /// swap file" where the text is already what the user wants but is not what is on disk.
    pub fn mark_dirty(&mut self) {
        self.revision += 1;
    }

    /// Replace the entire text in one operation, as a bulk substitute does. Callers take an undo
    /// checkpoint first.
    pub fn replace_all(&mut self, text: &str) {
        if self.read_only {
            return;
        }
        self.rope = Rope::from_str(text);
        self.revision += 1;
    }

    pub fn save(&mut self) -> io::Result<()> {
        match self.path.clone() {
            Some(path) => self.save_as(path),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no file name (use :w <file>)",
            )),
        }
    }

    /// Write the buffer to `path`, keeping its line endings and BOM. The write is atomic: content
    /// goes to a sibling file first and is renamed over the target, so a crash mid-write cannot
    /// truncate the user's file.
    pub fn save_as(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let mut bytes = Vec::new();
        if self.has_bom {
            bytes.extend_from_slice(BOM);
        }
        bytes.extend_from_slice(self.line_ending.apply(&self.rope.to_string()).as_bytes());

        let tmp = temp_sibling(path);
        fs::write(&tmp, &bytes)?;
        if let Ok(meta) = fs::metadata(path) {
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }
        match fs::rename(&tmp, path) {
            Ok(()) => {}
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                return Err(err);
            }
        }

        self.path = Some(path.to_path_buf());
        self.saved_revision = self.revision;
        Ok(())
    }
}

fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "nyavim".to_owned());
    let tmp_name = format!(".{}.nyavim.tmp", name);
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(tmp_name),
        _ => PathBuf::from(tmp_name),
    }
}

/// Order two positions the way text runs.
pub fn ordered(a: Pos, b: Pos) -> (Pos, Pos) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Word class used by the `w`/`b` motions: 0 whitespace, 1 word characters, 2 punctuation.
fn char_class(ch: char) -> u8 {
    if ch.is_whitespace() {
        0
    } else if ch.is_alphanumeric() || ch == '_' {
        1
    } else {
        2
    }
}

/// Match columns of `needle` in one line, left to right and non-overlapping.
///
/// `lowered` is `needle` with ASCII bytes lowercased, prepared once per scan: lowering per line
/// would mean an allocation per line.
fn slice_matches(
    slice: ropey::RopeSlice<'_>,
    needle: &str,
    lowered: &str,
    case_sensitive: bool,
) -> Vec<usize> {
    let mut found = Vec::new();
    if needle.is_empty() {
        return found;
    }

    if let Some(text) = slice.as_str() {
        return str_match_columns(text.trim_end_matches('\n'), needle, lowered, case_sensitive);
    }

    // A line straddling rope chunks: compare characters instead.
    let needle_chars: Vec<char> = needle.chars().collect();
    let mut chars: Vec<char> = slice.chars().collect();
    if chars.last() == Some(&'\n') {
        chars.pop();
    }
    let mut col = 0;
    while col + needle_chars.len() <= chars.len() {
        if matches_at(&chars, col, &needle_chars, case_sensitive) {
            found.push(col);
            col += needle_chars.len();
        } else {
            col += 1;
        }
    }
    found
}

/// Match columns of `needle` in one line of UTF-8 text.
fn str_match_columns(text: &str, needle: &str, lowered: &str, case_sensitive: bool) -> Vec<usize> {
    let mut found = Vec::new();
    let mut from = 0;
    let mut col = 0;

    if case_sensitive {
        while let Some(offset) = text[from..].find(needle) {
            let byte = from + offset;
            col += text[from..byte].chars().count();
            found.push(col);
            let end = byte + needle.len();
            col += text[byte..end].chars().count();
            from = end;
        }
        return found;
    }

    let haystack = text.to_ascii_lowercase();
    while let Some(offset) = haystack[from..].find(lowered) {
        let byte = from + offset;
        col += haystack[from..byte].chars().count();
        found.push(col);
        let end = byte + lowered.len();
        col += haystack[byte..end].chars().count();
        from = end;
    }
    found
}

fn lower_if_needed(needle: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        String::new()
    } else {
        needle.to_ascii_lowercase()
    }
}

/// Does `needle` start at `col` in `chars`? Used for lines that straddle rope chunks.
fn matches_at(chars: &[char], col: usize, needle: &[char], case_sensitive: bool) -> bool {
    needle.iter().enumerate().all(|(offset, expected)| {
        let actual = chars[col + offset];
        if actual == *expected {
            true
        } else if case_sensitive {
            false
        } else {
            actual.eq_ignore_ascii_case(expected)
        }
    })
}

/// Smart case: an all-lowercase query ignores case, any uppercase makes it exact.
pub fn is_case_sensitive(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "nyavim-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn crlf_and_bom_round_trip() {
        let dir = temp_dir("crlf");
        let path = dir.join("crlf.txt");
        fs::write(&path, b"\xEF\xBB\xBFone\r\ntwo\r\n").unwrap();

        let mut doc = Document::open_with_limit(&path, u64::MAX).unwrap();
        assert_eq!(doc.line_ending(), LineEnding::Crlf);
        assert!(doc.has_bom());
        assert_eq!(doc.line_count(), 2);
        assert_eq!(doc.line(0), "one");
        assert_eq!(doc.text(), "one\ntwo\n");

        let mut cursor = Pos::new(0, 0);
        doc.insert_char(&mut cursor, 'x');
        doc.remove(Pos::new(0, 0), Pos::new(0, 1));
        assert!(doc.is_dirty());
        doc.save().unwrap();
        assert!(!doc.is_dirty());
        assert_eq!(fs::read(&path).unwrap(), b"\xEF\xBB\xBFone\r\ntwo\r\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lf_without_trailing_newline_is_preserved() {
        let dir = temp_dir("lf");
        let path = dir.join("lf.txt");
        fs::write(&path, b"a\nb").unwrap();

        let mut doc = Document::open_with_limit(&path, u64::MAX).unwrap();
        assert_eq!(doc.line_ending(), LineEnding::Lf);
        assert_eq!(doc.line_count(), 2);
        doc.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a\nb");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_as_renames_the_buffer_and_clears_dirty() {
        let dir = temp_dir("saveas");
        let original = dir.join("original.txt");
        let copy = dir.join("copy.txt");
        fs::write(&original, b"hello\n").unwrap();

        let mut doc = Document::open_with_limit(&original, u64::MAX).unwrap();
        let mut cursor = Pos::new(0, 0);
        doc.insert_char(&mut cursor, 'x');
        doc.remove(Pos::new(0, 0), Pos::new(0, 1));
        doc.save_as(&copy).unwrap();

        assert_eq!(doc.display_name(), "copy.txt");
        assert!(!doc.is_dirty());
        assert_eq!(fs::read(&copy).unwrap(), b"hello\n");
        assert_eq!(fs::read(&original).unwrap(), b"hello\n");
        // no temp file left behind
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("nyavim.tmp"))
            .collect();
        assert!(leftovers.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_an_unnamed_buffer_is_an_error() {
        let mut doc = Document::empty();
        let err = doc.save().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn non_utf8_is_rejected() {
        let dir = temp_dir("binary");
        let path = dir.join("blob.bin");
        fs::write(&path, [0xff, 0xfe, 0x00, 0x41]).unwrap();
        let err = Document::open_with_limit(&path, u64::MAX).unwrap_err();
        assert!(matches!(err, LoadError::NotUtf8));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_unnamed_buffer_has_one_empty_line() {
        let doc = Document::empty();
        assert_eq!(doc.display_name(), "[No Name]");
        assert_eq!(doc.line_count(), 1);
        assert_eq!(doc.line(0), "");
    }

    #[test]
    fn insert_and_backspace_join_lines() {
        let mut doc = Document::from_text("ab\ncd");
        let mut cursor = Pos::new(1, 0);
        doc.backspace(&mut cursor);
        assert_eq!(doc.text(), "abcd");
        assert_eq!(cursor, Pos::new(0, 2));

        doc.insert_newline(&mut cursor);
        assert_eq!(doc.text(), "ab\ncd");
        assert_eq!(cursor, Pos::new(1, 0));
    }

    #[test]
    fn insert_char_advances_the_cursor() {
        let mut doc = Document::from_text("");
        let mut cursor = Pos::new(0, 0);
        doc.insert_char(&mut cursor, 'x');
        doc.insert_char(&mut cursor, 'y');
        assert_eq!(doc.text(), "xy");
        assert_eq!(cursor, Pos::new(0, 2));
        assert!(doc.is_dirty());
    }

    #[test]
    fn delete_forward_removes_the_character_under_the_cursor() {
        let mut doc = Document::from_text("abc");
        let mut cursor = Pos::new(0, 1);
        doc.delete_forward(&mut cursor);
        assert_eq!(doc.text(), "ac");
    }

    #[test]
    fn delete_line_keeps_at_least_one_line() {
        let mut doc = Document::from_text("one\ntwo\n");
        assert_eq!(doc.delete_line(0), "one\n");
        assert_eq!(doc.text(), "two\n");

        assert_eq!(doc.delete_line(0), "two\n");
        assert_eq!(doc.text(), "");
        assert_eq!(doc.line_count(), 1);
    }

    #[test]
    fn word_motions_cross_lines_and_skip_punctuation() {
        let doc = Document::from_text("foo bar_baz\nqux(1)");
        let cursor = Pos::new(0, 0);
        let cursor = doc.move_word_forward(cursor, 1);
        assert_eq!(cursor, Pos::new(0, 4));
        let cursor = doc.move_word_forward(cursor, 1);
        assert_eq!(cursor, Pos::new(1, 0));
        let cursor = doc.move_word_forward(cursor, 1);
        assert_eq!(cursor, Pos::new(1, 3));
        let cursor = doc.move_word_backward(cursor, 1);
        assert_eq!(cursor, Pos::new(1, 0));
        let cursor = doc.move_word_backward(cursor, 1);
        assert_eq!(cursor, Pos::new(0, 4));
    }

    #[test]
    fn vertical_motion_clamps_to_line_length() {
        let doc = Document::from_text("longer line\nshort\n");
        let cursor = Pos::new(0, 9);
        let cursor = doc.move_down(cursor, 1);
        assert_eq!(cursor, Pos::new(1, 4));
        let cursor = doc.move_down(cursor, 5);
        assert_eq!(cursor, Pos::new(1, 4));
        let cursor = doc.move_up(cursor, 1);
        assert_eq!(cursor.line, 0);
    }

    #[test]
    fn remove_and_slice_agree() {
        let mut doc = Document::from_text("hello world");
        let a = Pos::new(0, 0);
        let b = Pos::new(0, 5);
        assert_eq!(doc.slice(a, b), "hello");
        assert_eq!(doc.remove(a, b), "hello");
        assert_eq!(doc.text(), " world");
    }

    #[test]
    fn insert_lines_appends_at_the_end_without_a_double_newline() {
        let mut doc = Document::from_text("one\ntwo\n");
        doc.insert_lines(1, &["x".to_owned(), "y".to_owned()]);
        assert_eq!(doc.text(), "one\nx\ny\ntwo\n");
    }

    #[test]
    fn undo_and_redo_walk_the_history() {
        let mut doc = Document::from_text("one\n");
        let mut cursor = Pos::new(0, 0);
        doc.checkpoint(cursor);
        doc.insert_char(&mut cursor, 'X');
        assert_eq!(doc.text(), "Xone\n");

        let restored = doc.undo(cursor).expect("one undo step");
        assert_eq!(doc.text(), "one\n");
        assert_eq!(restored, Pos::new(0, 0));
        assert!(doc.undo(restored).is_none(), "history is exhausted");

        let redone = doc.redo(restored).expect("one redo step");
        assert_eq!(doc.text(), "Xone\n");
        assert!(doc.redo(redone).is_none());
    }

    #[test]
    fn undoing_back_to_the_saved_text_makes_the_buffer_clean() {
        let mut doc = Document::from_text("clean\n");
        let mut cursor = Pos::new(0, 0);
        doc.checkpoint(cursor);
        doc.insert_char(&mut cursor, 'X');
        assert!(doc.is_dirty());
        doc.undo(cursor).expect("undo");
        assert!(
            !doc.is_dirty(),
            "undoing back to the saved text is not a change"
        );
    }

    #[test]
    fn a_new_change_clears_the_redo_stack() {
        let mut doc = Document::from_text("a\n");
        let mut cursor = Pos::new(0, 0);
        doc.checkpoint(cursor);
        doc.insert_char(&mut cursor, 'b');
        cursor = doc.undo(cursor).expect("undo");
        doc.checkpoint(cursor);
        doc.insert_char(&mut cursor, 'c');
        assert!(
            doc.redo(cursor).is_none(),
            "a new change must drop the old redo path"
        );
    }

    #[test]
    fn find_forward_scans_the_rest_of_the_buffer() {
        let doc = Document::from_text("alpha beta\nbeta gamma\n");
        assert_eq!(
            doc.find_forward(Pos::new(0, 0), "beta", true),
            Some(Pos::new(0, 6))
        );
        assert_eq!(
            doc.find_forward(Pos::new(0, 7), "beta", true),
            Some(Pos::new(1, 0))
        );
        assert_eq!(doc.find_forward(Pos::new(1, 1), "beta", true), None);
    }

    #[test]
    fn find_backward_walks_up_to_the_start() {
        let doc = Document::from_text("alpha beta\nbeta gamma\n");
        assert_eq!(
            doc.find_backward(Pos::new(1, 5), "beta", true),
            Some(Pos::new(1, 0))
        );
        // inclusive: the match at the start position itself still counts
        assert_eq!(
            doc.find_backward(Pos::new(1, 0), "beta", true),
            Some(Pos::new(1, 0))
        );
        assert_eq!(
            doc.find_backward(Pos::new(0, 10), "beta", true),
            Some(Pos::new(0, 6))
        );
        assert_eq!(doc.find_backward(Pos::new(0, 5), "beta", true), None);
    }

    #[test]
    fn matches_are_non_overlapping_and_reported_per_line() {
        let doc = Document::from_text("aaaa\n");
        assert_eq!(doc.find_all_in_line(0, "aa", true), vec![0, 2]);
        assert_eq!(doc.find_all_in_line(0, "a", true), vec![0, 1, 2, 3]);
        assert!(doc.find_all_in_line(0, "b", true).is_empty());
    }

    #[test]
    fn smart_case_only_ignores_case_for_lowercase_queries() {
        assert!(!is_case_sensitive("beta"));
        assert!(is_case_sensitive("Beta"));
        assert!(is_case_sensitive("BETA"));

        let doc = Document::from_text("Beta beta\n");
        assert_eq!(doc.find_all_in_line(0, "beta", false), vec![0, 5]);
        assert_eq!(doc.find_all_in_line(0, "beta", true), vec![5]);
    }
}
