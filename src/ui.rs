//! Rendering. Reads [`App`] and draws; never mutates it.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::command;
use crate::mode::Mode;

/// Width of the command-name column in the palette.
const NAME_WIDTH: usize = 14;

/// Tab stops, in cells. Rendering and cursor placement must agree, so both go through the same
/// expansion.
pub const TAB_WIDTH: usize = 4;

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let rows = Layout::vertical([
        Constraint::Length(1), // tab bar
        Constraint::Min(1),    // buffer
        Constraint::Length(1), // status line
        Constraint::Length(1), // hint line
    ])
    .split(area);

    draw_tab_bar(frame, rows[0], app);
    draw_body(frame, rows[1], app);
    draw_status_line(frame, rows[2], app);
    draw_hint_line(frame, rows[3], app);

    match app.mode() {
        Mode::Palette => draw_palette(frame, rows[1], app),
        Mode::Help => draw_help(frame, rows[1]),
        _ => {}
    }
}

fn draw_tab_bar(frame: &mut Frame, area: Rect, app: &App) {
    let active = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let inactive = Style::default().fg(Color::Gray).bg(Color::DarkGray);
    let separator = Style::default().fg(Color::DarkGray);

    let mut spans = Vec::new();
    for (idx, doc) in app.buffers().iter().enumerate() {
        let dirty = if doc.is_dirty() { " ●" } else { "" };
        let label = format!(" {}{dirty} ", doc.display_name());
        spans.push(Span::styled(
            label,
            if idx == app.active() { active } else { inactive },
        ));
        spans.push(Span::styled("│", separator));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &App) {
    let doc = app.document();
    let cursor = app.cursor();
    let height = area.height as usize;
    let number_width = digits(doc.line_count()).max(3);

    let top = app.scroll().min(doc.line_count().saturating_sub(1));
    let bottom = (top + height).min(doc.line_count());

    let cursor_visible = !matches!(app.mode(), Mode::Help | Mode::Palette | Mode::Command);
    let highlight = app.search_highlight();
    let mut lines = Vec::with_capacity(height);
    for idx in top..bottom {
        let text = doc.line(idx);
        let cursor_col = if cursor_visible && idx == cursor.line {
            Some(cursor.col)
        } else {
            None
        };
        let marks = match highlight {
            Some((query, case_sensitive)) => {
                let cols = doc.find_all_in_line(idx, query, case_sensitive);
                marked_columns(&text, &cols, query.chars().count())
            }
            None => Vec::new(),
        };
        let mut spans = vec![Span::styled(
            format!("{:>number_width$} ", idx + 1, number_width = number_width),
            Style::default().fg(Color::DarkGray),
        )];
        spans.extend(line_spans(&text, cursor_col, &marks));
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Which characters of `text` belong to a search match, for highlighting.
fn marked_columns(text: &str, starts: &[usize], needle_len: usize) -> Vec<bool> {
    let total = text.chars().count();
    let mut marks = vec![false; total];
    for &start in starts {
        let end = (start + needle_len).min(total);
        for mark in &mut marks[start..end] {
            *mark = true;
        }
    }
    marks
}

/// Split one buffer line into spans: the cursor cell is reversed, matches are highlighted, and a
/// caret is drawn when the cursor sits past the last character (insert mode at the end of a line).
fn line_spans(text: &str, cursor_col: Option<usize>, marks: &[bool]) -> Vec<Span<'static>> {
    let caret = Style::default().add_modifier(Modifier::REVERSED);
    let hit = Style::default().fg(Color::Black).bg(Color::Yellow);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    let total = text.chars().count();

    for (idx, ch) in text.chars().enumerate() {
        let style = if Some(idx) == cursor_col {
            Some(caret)
        } else if marks.get(idx).copied().unwrap_or(false) {
            Some(hit)
        } else {
            None
        };
        if style != run_style {
            flush(&mut spans, &mut run, run_style);
            run_style = style;
        }
        if ch == '\t' {
            run.push_str(&" ".repeat(TAB_WIDTH));
        } else {
            run.push(ch);
        }
    }
    flush(&mut spans, &mut run, run_style);

    if cursor_col == Some(total) {
        spans.push(Span::styled(" ", caret));
    }
    spans
}

fn flush(spans: &mut Vec<Span<'static>>, run: &mut String, style: Option<Style>) {
    if run.is_empty() {
        return;
    }
    let text = std::mem::take(run);
    spans.push(match style {
        Some(style) => Span::styled(text, style),
        None => Span::raw(text),
    });
}

fn draw_status_line(frame: &mut Frame, area: Rect, app: &App) {
    let doc = app.document();
    let cursor = app.cursor();
    let mode = Span::styled(
        format!(" {} ", app.mode().label()),
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let file = Span::styled(
        format!(" {}", doc.display_name()),
        Style::default().add_modifier(Modifier::BOLD),
    );
    let position = Span::styled(
        format!("  {}:{}", cursor.line + 1, cursor.col + 1),
        Style::default().fg(Color::DarkGray),
    );
    let buffer_list = Span::styled(
        if app.buffers().len() > 1 {
            format!("  [{} of {}]", app.active() + 1, app.buffers().len())
        } else {
            String::new()
        },
        Style::default().fg(Color::DarkGray),
    );
    let ending = match doc.line_ending() {
        crate::document::LineEnding::Lf => "LF",
        crate::document::LineEnding::Crlf => "CRLF",
    };
    let details = Span::styled(
        format!("  {ending}{}", if doc.has_bom() { "+BOM" } else { "" }),
        Style::default().fg(Color::DarkGray),
    );

    let mut spans = vec![mode, file, buffer_list, position, details];
    if let Some((index, total, capped)) = app.search_progress() {
        let capped = if capped { "+" } else { "" };
        let text = if index == 0 {
            format!("  ?/{total}{capped}")
        } else {
            format!("  {index}/{total}{capped}")
        };
        spans.push(Span::styled(
            text,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(message) = app.message() {
        spans.push(Span::styled(
            format!("  {message}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_hint_line(frame: &mut Frame, area: Rect, app: &App) {
    if app.mode() == Mode::ReplaceConfirm {
        if let Some(prompt) = app.replace_prompt() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    prompt,
                    Style::default().fg(Color::Yellow),
                ))),
                area,
            );
            return;
        }
    }

    if matches!(app.mode(), Mode::Command | Mode::Search) {
        let (prefix, text) = if app.mode() == Mode::Search {
            ("/", app.search_query())
        } else {
            (":", app.command_line())
        };
        let mut spans = vec![
            Span::styled(prefix, Style::default().fg(Color::Cyan)),
            Span::raw(text.to_owned()),
            Span::styled("\u{2588}", Style::default().add_modifier(Modifier::REVERSED)),
        ];
        if app.mode() == Mode::Search {
            spans.push(Span::styled(
                "   Enter accept · Esc cancel",
                Style::default().fg(Color::DarkGray),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let mut spans = Vec::new();
    let pending = app.pending_keys();
    if !pending.is_empty() {
        spans.push(Span::styled(
            format!(" {pending}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            " …  ",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let hints: &[(&str, &str)] = match app.mode() {
        Mode::Insert => &[("Esc ", "normal"), ("Tab ", "indent"), ("Ctrl+K ", "palette")],
        Mode::Normal => &[
            ("i ", "insert"),
            (": ", "command"),
            ("Ctrl+K ", "palette"),
            ("F1 ", "help"),
        ],
        _ => &[("Esc ", "back"), ("F1 ", "help")],
    };
    for (key, label) in hints {
        spans.push(Span::styled(*key, Style::default().fg(Color::Cyan)));
        spans.push(Span::styled(*label, Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw("  "));
    }
    if app.mode() == Mode::Normal && app.buffers().len() > 1 {
        spans.push(Span::styled(
            "Ctrl+N/P ",
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled(
            "files",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A palette overlay listing every command the query matches, best first.
fn draw_palette(frame: &mut Frame, area: Rect, app: &App) {
    let matches = app.palette_matches();
    let width = area.width.saturating_sub(4).clamp(24, 72);
    let height = (matches.len() as u16 + 3).min(area.height);
    let overlay = centered(area, width, height);
    if overlay.width < 8 || overlay.height < 4 {
        return;
    }

    frame.render_widget(Clear, overlay);
    let block = Block::bordered().title(" Commands ");
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);
    if inner.height < 2 {
        return;
    }

    let query_row = Rect { height: 1, ..inner };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(app.palette_query().to_owned()),
            Span::styled("\u{2588}", Style::default().add_modifier(Modifier::REVERSED)),
        ])),
        query_row,
    );

    let list_area = Rect {
        y: inner.y + 1,
        height: inner.height - 1,
        ..inner
    };
    let visible = list_area.height as usize;
    let selected = app.palette_selected();
    let offset = selected.saturating_sub(visible.saturating_sub(1));

    let mut lines = Vec::new();
    for (idx, cmd) in matches.iter().enumerate().skip(offset).take(visible) {
        let chosen = idx == selected;
        let style = if chosen {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let keys_width = cmd.keys.chars().count() + 2;
        let desc_width = (inner.width as usize).saturating_sub(1 + NAME_WIDTH + keys_width);
        let description: String = cmd.description.chars().take(desc_width).collect();

        let mut row = vec![Span::styled(
            format!(" {:<NAME_WIDTH$}", cmd.name),
            style.add_modifier(Modifier::BOLD),
        )];
        row.push(Span::styled(
            format!("{:<desc_width$}", description),
            style,
        ));
        row.push(Span::styled(
            format!(" {} ", cmd.keys),
            style.fg(if chosen { Color::Black } else { Color::DarkGray }),
        ));
        lines.push(Line::from(row));
    }
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// The `F1` page: every key that works and every command that can be run.
fn draw_help(frame: &mut Frame, _area: Rect) {
    let area = frame.area();
    let width = area.width.saturating_sub(6).clamp(30, 78);
    let entries = command::KEYS.len() + command::COMMANDS.len();
    let height = ((entries + 4) as u16).min(area.height);
    let overlay = centered(area, width, height);
    if overlay.width < 12 || overlay.height < 6 {
        return;
    }

    frame.render_widget(Clear, overlay);
    let block = Block::bordered().title(" Keys and commands  ·  F1 or Esc to close ");
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        " Keys",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    for (keys, description) in command::KEYS {
        lines.push(Line::from(vec![
            Span::raw(format!("  {keys:<14}")),
            Span::styled(*description, Style::default().fg(Color::Gray)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Commands",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    for cmd in command::COMMANDS {
        lines.push(Line::from(vec![
            Span::raw(format!("  {:<14}", cmd.keys)),
            Span::styled(cmd.description, Style::default().fg(Color::Gray)),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn digits(n: usize) -> usize {
    let mut n = n.max(1);
    let mut count = 0;
    while n > 0 {
        count += 1;
        n /= 10;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::draw;
    use crate::app::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;

    /// Render one frame headlessly and return it as plain text, one screen row per line.
    fn screen(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();

        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn app_with(text: &str) -> App {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let dir = std::env::temp_dir().join(format!(
            "nyavim-ui-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shown.txt");
        fs::write(&path, text).unwrap();

        let mut app = App::new();
        app.open(&path);
        app
    }

    #[test]
    fn renders_the_buffer_with_line_numbers_and_the_status_line() {
        let app = app_with("alpha\nbeta\n");
        let screen = screen(&app, 40, 8);
        assert!(screen.contains("shown.txt"), "tab bar shows the file name: {screen}");
        assert!(screen.contains("alpha"));
        assert!(screen.contains("beta"));
        assert!(screen.contains("NORMAL"), "status line shows the mode: {screen}");
        assert!(screen.contains("1:1"));
        assert!(screen.contains("LF"));
    }

    #[test]
    fn a_dirty_buffer_is_marked_in_the_tab_bar() {
        let mut app = app_with("alpha\n");
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('x'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        let screen = screen(&app, 40, 8);
        assert!(screen.contains('●'), "unsaved buffers show a dot: {screen}");
        assert!(screen.contains("INSERT"));
    }

    #[test]
    fn the_palette_overlay_lists_commands() {
        let mut app = app_with("alpha\n");
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('k'),
            ratatui::crossterm::event::KeyModifiers::CONTROL,
        ));
        let screen = screen(&app, 70, 20);
        assert!(screen.contains("Commands"), "palette box: {screen}");
        assert!(screen.contains("write"));
        assert!(screen.contains("quit"));
    }

    #[test]
    fn the_help_page_lists_keys_and_commands() {
        let mut app = app_with("alpha\n");
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::F(1),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        let screen = screen(&app, 80, 40);
        assert!(screen.contains("Keys"));
        assert!(screen.contains("Commands"));
        assert!(screen.contains("Ctrl+K"));
    }

    #[test]
    fn the_command_line_is_drawn_on_the_last_row() {
        let mut app = app_with("alpha\n");
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char(':'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        for ch in "write".chars() {
            app.handle_key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(ch),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }
        let screen = screen(&app, 40, 8);
        let last = screen.lines().nth(7).unwrap_or_default();
        assert!(last.contains(":write"), "command line row: {last:?}");
    }

    #[test]
    fn search_matches_are_highlighted() {
        use ratatui::style::Color;

        let mut app = app_with("alpha beta\nbeta\n");
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('/'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        for ch in "beta".chars() {
            app.handle_key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(ch),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }

        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();

        // The cursor jumped onto the first match, so that cell is the caret; the match on the
        // second line shows the plain highlight. Body starts on row 1, gutter is 4 cells wide.
        let cursor_cell = &buffer[(4 + 6, 1)];
        assert_eq!(
            cursor_cell.style().add_modifier,
            ratatui::style::Modifier::REVERSED,
            "the match under the cursor is the caret"
        );
        let hit_cell = &buffer[(4, 2)];
        assert_eq!(hit_cell.style().bg, Some(Color::Yellow), "other matches are highlighted");
        assert_eq!(hit_cell.symbol(), "b");

        let mut last_row = String::new();
        for x in 0..40 {
            last_row.push_str(buffer[(x, 7)].symbol());
        }
        assert!(last_row.contains("beta"), "prompt row shows the query: {last_row:?}");
    }
}
