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
    cursor: Pos,
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
            cursor: Pos::default(),
            path: None,
            line_ending: LineEnding::Lf,
            has_bom: false,
            revision: 0,
            saved_revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
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

    pub fn open(path: impl AsRef<Path>) -> Result<Self, LoadError> {
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

    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    /// Remember the current state as the point `undo` returns to. Call this once before a change
    /// (one insert session, one operator), not per character.
    pub fn checkpoint(&mut self) {
        self.undo.push(self.snapshot());
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

    /// Step back to the last checkpoint. False when there is nothing left to undo.
    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.snapshot());
        self.apply(previous);
        true
    }

    /// Step forward again. False when there is nothing to redo.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.snapshot());
        self.apply(next);
        true
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.rope.to_string(),
            cursor: self.cursor,
            revision: self.revision,
        }
    }

    fn apply(&mut self, snapshot: Snapshot) {
        self.rope = Rope::from_str(&snapshot.text);
        self.revision = snapshot.revision;
        self.cursor = self.clamp(snapshot.cursor);
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

    // ---- cursor -------------------------------------------------------

    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    /// Move the cursor, clamping it into the buffer.
    pub fn set_cursor(&mut self, pos: Pos) {
        self.cursor = self.clamp(pos);
    }

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

    /// Keep the cursor on a character when in normal mode, where sitting past the last character
    /// would show a cursor that cannot be acted on.
    pub fn clamp_normal(&mut self) {
        let len = self.line_len(self.cursor.line);
        self.cursor.col = if len == 0 {
            0
        } else {
            self.cursor.col.min(len - 1)
        };
    }

    fn clamp(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.line_count().saturating_sub(1));
        let col = pos.col.min(self.line_len(line));
        Pos { line, col }
    }

    fn char_index(&self, pos: Pos) -> usize {
        self.rope.line_to_char(pos.line) + pos.col
    }

    // ---- primitive text operations ------------------------------------

    /// Text between two positions, ordered low to high.
    pub fn slice(&self, a: Pos, b: Pos) -> String {
        let (a, b) = ordered(self.clamp(a), self.clamp(b));
        self.rope.slice(self.char_index(a)..self.char_index(b)).to_string()
    }

    /// Remove and return the text between two positions.
    pub fn remove(&mut self, a: Pos, b: Pos) -> String {
        let (a, b) = ordered(self.clamp(a), self.clamp(b));
        let (from, to) = (self.char_index(a), self.char_index(b));
        if from >= to {
            return String::new();
        }
        let removed = self.rope.slice(from..to).to_string();
        self.rope.remove(from..to);
        self.revision += 1;
        self.cursor = self.clamp(self.cursor);
        removed
    }

    /// Insert text at a position without moving the cursor.
    pub fn insert_at(&mut self, pos: Pos, text: &str) {
        if text.is_empty() {
            return;
        }
        let at = self.clamp(pos);
        let idx = self.char_index(at);
        self.rope.insert(idx, text);
        self.revision += 1;
    }

    pub fn insert_char(&mut self, ch: char) {
        let idx = self.char_index(self.cursor);
        self.rope.insert_char(idx, ch);
        self.revision += 1;
        self.cursor.col += 1;
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
        self.cursor.line += 1;
        self.cursor.col = 0;
    }

    /// Delete the character before the cursor, joining lines when at the start of one.
    pub fn backspace(&mut self) {
        if self.cursor.col > 0 {
            let from = Pos::new(self.cursor.line, self.cursor.col - 1);
            self.remove(from, self.cursor);
            self.cursor.col -= 1;
        } else if self.cursor.line > 0 {
            let prev = self.cursor.line - 1;
            let join = Pos::new(prev, self.line_len(prev));
            self.remove(join, self.cursor);
            self.cursor = join;
        }
    }

    /// Delete the character under the cursor, joining lines when at the end of one.
    pub fn delete_forward(&mut self) {
        let len = self.line_len(self.cursor.line);
        let to = if self.cursor.col < len {
            Pos::new(self.cursor.line, self.cursor.col + 1)
        } else if self.cursor.line + 1 < self.line_count() {
            Pos::new(self.cursor.line + 1, 0)
        } else {
            return;
        };
        self.remove(self.cursor, to);
        self.clamp_normal();
    }

    /// Remove a whole line including one of its line endings; returns the removed text.
    ///
    /// Works on raw rope lines so that deleting the last visible line of a file that ends with a
    /// newline removes that newline too instead of leaving a stray empty line behind.
    pub fn delete_line(&mut self, line: usize) -> String {
        if line >= self.line_count() {
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
        self.cursor.line = self.cursor.line.min(self.line_count().saturating_sub(1));
        self.cursor = self.clamp(self.cursor);
        self.clamp_normal();
        removed
    }

    /// Insert lines starting at `line`, as a linewise paste does. A `line` past the last line
    /// appends at the end of the buffer.
    pub fn insert_lines(&mut self, line: usize, lines: &[String]) {
        if lines.is_empty() {
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

    // ---- motions ------------------------------------------------------

    /// `h`: never leaves the line, like vim.
    pub fn move_left(&mut self, count: usize) {
        self.cursor.col = self.cursor.col.saturating_sub(count);
    }

    /// `l`.
    pub fn move_right(&mut self, count: usize) {
        self.cursor.col = (self.cursor.col + count).min(self.line_len(self.cursor.line));
        self.clamp_normal();
    }

    /// `j`.
    pub fn move_down(&mut self, count: usize) {
        self.cursor.line = (self.cursor.line + count).min(self.line_count().saturating_sub(1));
        self.clamp_normal();
    }

    /// `k`.
    pub fn move_up(&mut self, count: usize) {
        self.cursor.line = self.cursor.line.saturating_sub(count);
        self.clamp_normal();
    }

    /// `$`.
    pub fn move_line_end(&mut self) {
        self.cursor.col = self.line_len(self.cursor.line).saturating_sub(1);
    }

    /// `w`: start of the next word, crossing lines.
    pub fn move_word_forward(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            self.skip_space_forward();
            if self.at_end_of_text() {
                break;
            }
            let class = char_class(self.char_at(self.cursor).unwrap_or(' '));
            while let Some(ch) = self.char_at(self.cursor) {
                if char_class(ch) != class {
                    break;
                }
                self.cursor.col += 1;
            }
            self.skip_space_forward();
        }
        self.clamp_normal();
    }

    /// `b`: start of the previous word, crossing lines.
    pub fn move_word_backward(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            if !self.step_back_one() {
                break;
            }
            self.skip_space_backward();
            let class = match self.char_at(self.cursor) {
                Some(ch) => char_class(ch),
                None => continue,
            };
            while self.cursor.col > 0 {
                match self.char_at(Pos::new(self.cursor.line, self.cursor.col - 1)) {
                    Some(prev) if char_class(prev) == class => self.cursor.col -= 1,
                    _ => break,
                }
            }
        }
        self.clamp_normal();
    }

    /// Move one character back, crossing to the end of the previous line. False at the start of
    /// the buffer.
    fn step_back_one(&mut self) -> bool {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
            true
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.col = self.line_len(self.cursor.line).saturating_sub(1);
            true
        } else {
            false
        }
    }

    fn at_end_of_text(&self) -> bool {
        self.cursor.line + 1 >= self.line_count()
            && self.cursor.col >= self.line_len(self.cursor.line)
    }

    fn char_at(&self, pos: Pos) -> Option<char> {
        if pos.line >= self.line_count() || pos.col >= self.line_len(pos.line) {
            return None;
        }
        Some(self.rope.line(pos.line).char(pos.col))
    }

    fn skip_space_forward(&mut self) {
        loop {
            match self.char_at(self.cursor) {
                Some(ch) if ch.is_whitespace() => self.cursor.col += 1,
                Some(_) => break,
                None => {
                    if self.cursor.line + 1 < self.line_count() {
                        self.cursor.line += 1;
                        self.cursor.col = 0;
                    } else {
                        self.cursor.col = self.line_len(self.cursor.line);
                        break;
                    }
                }
            }
        }
    }

    fn skip_space_backward(&mut self) {
        loop {
            if self.cursor.col == 0 {
                if self.cursor.line == 0 {
                    break;
                }
                self.cursor.line -= 1;
                self.cursor.col = self.line_len(self.cursor.line);
                continue;
            }
            let prev = self.char_at(Pos::new(self.cursor.line, self.cursor.col - 1));
            match prev {
                Some(ch) if ch.is_whitespace() => self.cursor.col -= 1,
                _ => break,
            }
        }
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

        let mut doc = Document::open(&path).unwrap();
        assert_eq!(doc.line_ending(), LineEnding::Crlf);
        assert!(doc.has_bom());
        assert_eq!(doc.line_count(), 2);
        assert_eq!(doc.line(0), "one");
        assert_eq!(doc.text(), "one\ntwo\n");

        doc.insert_char('x');
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

        let mut doc = Document::open(&path).unwrap();
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

        let mut doc = Document::open(&original).unwrap();
        doc.insert_char('x');
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
        let err = Document::open(&path).unwrap_err();
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
        doc.set_cursor(Pos::new(1, 0));
        doc.backspace();
        assert_eq!(doc.text(), "abcd");
        assert_eq!(doc.cursor(), Pos::new(0, 2));

        doc.insert_newline();
        assert_eq!(doc.text(), "ab\ncd");
        assert_eq!(doc.cursor(), Pos::new(1, 0));
    }

    #[test]
    fn insert_char_advances_the_cursor() {
        let mut doc = Document::from_text("");
        doc.insert_char('x');
        doc.insert_char('y');
        assert_eq!(doc.text(), "xy");
        assert_eq!(doc.cursor(), Pos::new(0, 2));
        assert!(doc.is_dirty());
    }

    #[test]
    fn delete_forward_removes_the_character_under_the_cursor() {
        let mut doc = Document::from_text("abc");
        doc.set_cursor(Pos::new(0, 1));
        doc.delete_forward();
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
        let mut doc = Document::from_text("foo bar_baz\nqux(1)");
        doc.set_cursor(Pos::new(0, 0));
        doc.move_word_forward(1);
        assert_eq!(doc.cursor(), Pos::new(0, 4));
        doc.move_word_forward(1);
        assert_eq!(doc.cursor(), Pos::new(1, 0));
        doc.move_word_forward(1);
        assert_eq!(doc.cursor(), Pos::new(1, 3));
        doc.move_word_backward(1);
        assert_eq!(doc.cursor(), Pos::new(1, 0));
        doc.move_word_backward(1);
        assert_eq!(doc.cursor(), Pos::new(0, 4));
    }

    #[test]
    fn vertical_motion_clamps_to_line_length() {
        let mut doc = Document::from_text("longer line\nshort\n");
        doc.set_cursor(Pos::new(0, 9));
        doc.move_down(1);
        assert_eq!(doc.cursor(), Pos::new(1, 4));
        doc.move_down(5);
        assert_eq!(doc.cursor(), Pos::new(1, 4));
        doc.move_up(1);
        assert_eq!(doc.cursor().line, 0);
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
        doc.checkpoint();
        doc.insert_char('X');
        assert_eq!(doc.text(), "Xone\n");

        assert!(doc.undo());
        assert_eq!(doc.text(), "one\n");
        assert!(!doc.undo(), "history is exhausted");

        assert!(doc.redo());
        assert_eq!(doc.text(), "Xone\n");
        assert!(!doc.redo());
    }

    #[test]
    fn undoing_back_to_the_saved_text_makes_the_buffer_clean() {
        let mut doc = Document::from_text("clean\n");
        doc.checkpoint();
        doc.insert_char('X');
        assert!(doc.is_dirty());
        doc.undo();
        assert!(!doc.is_dirty(), "undoing back to the saved text is not a change");
    }

    #[test]
    fn a_new_change_clears_the_redo_stack() {
        let mut doc = Document::from_text("a\n");
        doc.checkpoint();
        doc.insert_char('b');
        doc.undo();
        doc.checkpoint();
        doc.insert_char('c');
        assert!(!doc.redo(), "a new change must drop the old redo path");
    }

}
