//! Rendering. Reads [`App`] and draws; never mutates it.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::command;
use crate::config::Theme;
use crate::document::Pos;
use crate::layout::SplitDir;
use crate::mode::Mode;
use crate::syntax::{self, Language, TokenKind};

/// Width of the command-name column in the palette.
const NAME_WIDTH: usize = 14;

/// Tab stops, in cells. Rendering and cursor placement must agree, so both go through the same
/// expansion.
pub const TAB_WIDTH: usize = 4;

pub fn draw(frame: &mut Frame, app: &App) {
    if app.mode() == Mode::Terminal {
        draw_terminal(frame, app);
        return;
    }
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
        Mode::Help => draw_help(frame, app),
        _ => {}
    }
}

/// Text of one tab, shared by the renderer and by mouse hit-testing so the two cannot drift.
pub fn tab_label(doc: &crate::document::Document) -> String {
    let dirty = if doc.is_dirty() { " ●" } else { "" };
    format!(" {}{dirty} ", doc.display_name())
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
        let label = tab_label(doc);
        spans.push(Span::styled(
            label,
            if idx == app.active() {
                active
            } else {
                inactive
            },
        ));
        spans.push(Span::styled("│", separator));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(frame: &mut Frame, _area: Rect, app: &App) {
    let highlight = app.search_highlight();
    let focused = app.focused_window();

    for (id, rect) in app.window_rects() {
        let Some(window) = app.window_snapshot(id) else {
            continue;
        };
        let Some(doc) = app.buffer(window.buffer) else {
            continue;
        };
        draw_window(
            frame,
            rect,
            &PaneView {
                doc,
                window,
                focused: id == focused,
                mode: app.mode(),
                highlight,
                selection: app.selection(),
                git: app.git_info_for(window.buffer),
                theme: app.config().theme,
                tab_width: app.config().tab_width,
            },
        );
    }

    for (direction, rect) in app.separators() {
        draw_separator(frame, direction, rect);
    }
}

/// Paint the shell's screen cell by cell, then put the terminal cursor where the shell wants it.
fn draw_terminal(frame: &mut Frame, app: &App) {
    let Some(session) = app.terminal() else {
        return;
    };
    let screen = session.screen();
    let area = frame.area();
    let body_height = area.height.saturating_sub(1);
    let (rows, cols) = screen.size();

    {
        let buffer = frame.buffer_mut();
        for y in 0..rows.min(body_height) {
            for x in 0..cols.min(area.width) {
                let Some(cell) = screen.cell(y, x) else {
                    continue;
                };
                let contents = cell.contents();
                if contents.is_empty() {
                    continue;
                }
                let mut style = Style::default();
                if let Some(color) = vt_color(cell.fgcolor()) {
                    style = style.fg(color);
                }
                if let Some(color) = vt_color(cell.bgcolor()) {
                    style = style.bg(color);
                }
                let mut modifier = Modifier::empty();
                if cell.bold() {
                    modifier |= Modifier::BOLD;
                }
                if cell.italic() {
                    modifier |= Modifier::ITALIC;
                }
                if cell.underline() {
                    modifier |= Modifier::UNDERLINED;
                }
                if cell.inverse() {
                    modifier |= Modifier::REVERSED;
                }
                buffer.set_string(
                    area.x + x,
                    area.y + y,
                    &contents,
                    style.add_modifier(modifier),
                );
            }
        }
    }

    let (cursor_y, cursor_x) = screen.cursor_position();
    if !screen.hide_cursor() && cursor_y < body_height && cursor_x < area.width {
        frame.set_cursor_position((area.x + cursor_x, area.y + cursor_y));
    }

    let hint_area = Rect {
        x: area.x,
        y: area.y + body_height,
        width: area.width,
        height: 1,
    };
    let hint = Line::from(vec![
        Span::styled(" Ctrl+\\ ", Style::default().fg(Color::Cyan)),
        Span::styled(
            "back to the editor  ·  the shell owns this screen",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}

fn vt_color(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(Color::Indexed(index)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

fn draw_separator(frame: &mut Frame, direction: SplitDir, area: Rect) {
    let style = Style::default().fg(Color::DarkGray);
    let symbol = match direction {
        SplitDir::SideBySide => "│",
        SplitDir::Stacked => "─",
    };
    let buffer = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            buffer.set_string(x, y, symbol, style);
        }
    }
}

/// Everything one pane needs, bundled so the renderer keeps a readable signature.
struct PaneView<'a> {
    doc: &'a crate::document::Document,
    window: &'a crate::app::Window,
    focused: bool,
    mode: Mode,
    highlight: Option<(&'a str, bool)>,
    selection: Option<(Pos, Pos)>,
    git: Option<&'a crate::git::GitInfo>,
    theme: Theme,
    tab_width: usize,
}

fn draw_window(frame: &mut Frame, area: Rect, view: &PaneView<'_>) {
    let PaneView {
        doc,
        window,
        focused,
        mode,
        highlight,
        selection,
        git,
        theme,
        tab_width,
    } = *view;
    let cursor = window.cursor;
    let height = area.height as usize;
    if height == 0 || area.width == 0 {
        return;
    }
    let number_width = digits(doc.line_count()).max(3);
    let language = Language::detect(doc.path());
    let show_marks = git.is_some_and(|info| info.in_repo());

    let top = window.scroll.min(doc.line_count().saturating_sub(1));
    let bottom = (top + height).min(doc.line_count());

    let cursor_visible = focused
        && !matches!(
            mode,
            Mode::Help | Mode::Palette | Mode::Command | Mode::ReplaceConfirm | Mode::WindowCmd
        );
    let gutter_style = if focused {
        Style::default().fg(Color::Gray)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let mut state = window.highlight;
    let mut lines = Vec::with_capacity(height);
    for idx in top..bottom {
        let text = doc.line(idx);
        let (tokens, next_state) = syntax::highlight_line(language, &text, state);
        state = next_state;

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
        let selected = selection.and_then(|(start, end)| {
            if idx < start.line || idx > end.line {
                None
            } else {
                let from = if idx == start.line { start.col } else { 0 };
                let to = if idx == end.line {
                    end.col
                } else {
                    text.chars().count()
                };
                Some((from, to))
            }
        });
        let mut spans = Vec::new();
        if show_marks {
            spans.push(mark_span(
                git.and_then(|info| info.marks.get(idx).copied())
                    .unwrap_or(crate::git::Mark::Unchanged),
                doc.is_dirty(),
            ));
        }
        spans.push(Span::styled(
            format!("{:>number_width$} ", idx + 1, number_width = number_width),
            gutter_style,
        ));
        spans.extend(line_spans(
            &text, cursor_col, &marks, &tokens, theme, tab_width, selected,
        ));
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// One gutter cell describing the line's change against HEAD.
///
/// When the buffer has unsaved edits the marks include those edits, so they are drawn bold to say
/// "this is not only what is on disk".
fn mark_span(mark: crate::git::Mark, dirty: bool) -> Span<'static> {
    let (symbol, color) = match mark {
        crate::git::Mark::Unchanged => (" ", Color::Reset),
        crate::git::Mark::Added => ("+", Color::Green),
        crate::git::Mark::Modified => ("~", Color::Yellow),
        crate::git::Mark::RemovedAbove => ("_", Color::Red),
    };
    let mut style = Style::default().fg(color);
    if dirty && mark != crate::git::Mark::Unchanged {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(symbol, style)
}

/// Colour for a token, using the terminal's own palette so it follows the user's theme.
fn token_style(kind: TokenKind, theme: Theme) -> Style {
    match kind {
        TokenKind::Keyword => Style::default()
            .fg(theme.keyword)
            .add_modifier(Modifier::BOLD),
        TokenKind::String => Style::default().fg(theme.string),
        TokenKind::Comment => Style::default().fg(theme.comment),
        TokenKind::Number => Style::default().fg(theme.number),
        TokenKind::Type => Style::default().fg(theme.type_name),
        TokenKind::Heading => Style::default()
            .fg(theme.heading)
            .add_modifier(Modifier::BOLD),
        TokenKind::Link => Style::default()
            .fg(theme.link)
            .add_modifier(Modifier::UNDERLINED),
    }
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

/// Split one buffer line into spans: tokens get their colour, the cursor cell is reversed, matches
/// are highlighted, and a caret is drawn when the cursor sits past the last character.
///
/// Precedence is deliberate: syntax colour is the base, a search hit replaces its background, and
/// the cursor sits on top of both so it never disappears.
fn line_spans(
    text: &str,
    cursor_col: Option<usize>,
    marks: &[bool],
    tokens: &[(usize, usize, TokenKind)],
    theme: Theme,
    tab_width: usize,
    selection: Option<(usize, usize)>,
) -> Vec<Span<'static>> {
    let caret = Style::default().add_modifier(Modifier::REVERSED);
    let mut token_kinds: Vec<Option<TokenKind>> = vec![None; text.chars().count()];
    let total_chars = token_kinds.len();
    for (start, end, kind) in tokens {
        let end = (*end).min(total_chars);
        if *start < end {
            token_kinds[*start..end].fill(Some(*kind));
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    let total = text.chars().count();

    for (idx, ch) in text.chars().enumerate() {
        let base = token_kinds[idx].map(|kind| token_style(kind, theme));
        let style = if Some(idx) == cursor_col {
            Some(caret)
        } else if selection.is_some_and(|(from, to)| idx >= from && idx < to) {
            Some(Style::default().bg(Color::Blue).fg(Color::White))
        } else if marks.get(idx).copied().unwrap_or(false) {
            let foreground = token_kinds[idx]
                .map(|kind| token_style(kind, theme))
                .and_then(|style| style.fg)
                .unwrap_or(Color::Black);
            Some(Style::default().fg(foreground).bg(Color::Yellow))
        } else {
            base
        };
        if style != run_style {
            flush(&mut spans, &mut run, run_style);
            run_style = style;
        }
        if ch == '\t' {
            run.push_str(&" ".repeat(tab_width));
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
    let language = Span::styled(
        format!("  {}", app.language().label()),
        Style::default().fg(Color::DarkGray),
    );
    let read_only = Span::styled(
        if doc.is_read_only() { "  RO" } else { "" },
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    );
    let branch = Span::styled(
        match app.git_info().branch.as_deref() {
            Some(name) => format!("  {name}"),
            None => String::new(),
        },
        Style::default().fg(Color::Blue),
    );

    let mut spans = vec![
        mode,
        file,
        buffer_list,
        position,
        details,
        language,
        read_only,
        branch,
    ];
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

    if app.swap_prompt().is_some() {
        let line = Line::from(vec![
            Span::styled(
                " recovered edits ",
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ),
            Span::styled("  (r)", Style::default().fg(Color::Cyan)),
            Span::styled("ecover   ", Style::default().fg(Color::DarkGray)),
            Span::styled("(d)", Style::default().fg(Color::Cyan)),
            Span::styled("iscard", Style::default().fg(Color::DarkGray)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    if app.mode() == Mode::CommitMsg {
        let line = Line::from(vec![
            Span::styled("commit ", Style::default().fg(Color::Cyan)),
            Span::raw(app.commit_message().to_owned()),
            Span::styled(
                "\u{2588}",
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Span::styled(
                "   Enter to commit · Esc to cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    if matches!(app.mode(), Mode::Command | Mode::Search) {
        let (prefix, text) = if app.mode() == Mode::Search {
            (app.search_prefix(), app.search_query())
        } else {
            (":", app.command_line())
        };
        let mut spans = vec![
            Span::styled(prefix, Style::default().fg(Color::Cyan)),
            Span::raw(text.to_owned()),
            Span::styled(
                "\u{2588}",
                Style::default().add_modifier(Modifier::REVERSED),
            ),
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
        spans.push(Span::styled(" …  ", Style::default().fg(Color::DarkGray)));
    }
    let hints: &[(&str, &str)] = match app.mode() {
        Mode::Insert => &[
            ("Esc ", "normal"),
            ("Tab ", "indent"),
            ("Ctrl+K ", "palette"),
        ],
        Mode::WindowCmd => &[
            ("h j k l ", "move focus"),
            ("w ", "cycle"),
            ("c ", "close"),
            ("o ", "only this"),
        ],
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
        spans.push(Span::styled("Ctrl+N/P ", Style::default().fg(Color::Cyan)));
        spans.push(Span::styled("files", Style::default().fg(Color::DarkGray)));
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
            Span::styled(
                "\u{2588}",
                Style::default().add_modifier(Modifier::REVERSED),
            ),
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
        row.push(Span::styled(format!("{:<desc_width$}", description), style));
        row.push(Span::styled(
            format!(" {} ", cmd.keys),
            style.fg(if chosen {
                Color::Black
            } else {
                Color::DarkGray
            }),
        ));
        lines.push(Line::from(row));
    }
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// The `F1` page: every key that works and every command that can be run.
fn draw_help(frame: &mut Frame, app: &App) {
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
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
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
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    for cmd in command::COMMANDS {
        lines.push(Line::from(vec![
            Span::raw(format!("  {:<14}", cmd.keys)),
            Span::styled(cmd.description, Style::default().fg(Color::Gray)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Syntax highlighting follows the file extension; the status line shows what was detected.",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        match app.config().path.as_deref() {
            Some(path) => format!(" Config: {}", path.display()),
            None => " Config: none".to_owned(),
        },
        Style::default().fg(Color::DarkGray),
    )));

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

/// Digits needed for line numbers, shared with the click mapping.
pub fn digits(n: usize) -> usize {
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

    fn app_named(name: &str, text: &str) -> App {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let dir = std::env::temp_dir().join(format!(
            "nyavim-ui-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, text).unwrap();

        let mut app = App::new();
        app.open(&path);
        app
    }

    fn app_with(text: &str) -> App {
        app_named("shown.txt", text)
    }

    #[test]
    fn renders_the_buffer_with_line_numbers_and_the_status_line() {
        let app = app_with("alpha\nbeta\n");
        let screen = screen(&app, 40, 8);
        assert!(
            screen.contains("shown.txt"),
            "tab bar shows the file name: {screen}"
        );
        assert!(screen.contains("alpha"));
        assert!(screen.contains("beta"));
        assert!(
            screen.contains("NORMAL"),
            "status line shows the mode: {screen}"
        );
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
        assert_eq!(
            hit_cell.style().bg,
            Some(Color::Yellow),
            "other matches are highlighted"
        );
        assert_eq!(hit_cell.symbol(), "b");

        let mut last_row = String::new();
        for x in 0..40 {
            last_row.push_str(buffer[(x, 7)].symbol());
        }
        assert!(
            last_row.contains("beta"),
            "prompt row shows the query: {last_row:?}"
        );
    }

    #[test]
    fn a_split_draws_two_panes_with_a_separator() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app_with("alpha\nbeta\n");
        app.set_area(40, 10);
        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        for ch in "vsp".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let screen = screen(&app, 40, 10);
        assert!(
            screen.lines().nth(1).unwrap_or_default().contains('│'),
            "a separator column is drawn between panes:\n{screen}"
        );
        assert!(
            screen.matches("alpha").count() >= 2,
            "both panes show the buffer:\n{screen}"
        );
    }

    #[test]
    fn rust_keywords_numbers_and_comments_are_coloured() {
        use ratatui::style::Color;

        let mut app = app_named("code.rs", "let x = 1; // note\n");
        app.set_area(40, 8);
        // put the caret on `x` so the keyword cell is not covered by the cursor
        for _ in 0..4 {
            app.handle_key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char('l'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();

        // gutter is 4 cells wide on row 1
        assert_eq!(buffer[(4, 1)].symbol(), "l");
        assert_eq!(
            buffer[(4, 1)].style().fg,
            Some(Color::Magenta),
            "keyword is magenta"
        );
        assert_eq!(
            buffer[(12, 1)].style().fg,
            Some(Color::Yellow),
            "the number is yellow"
        );
        assert_eq!(
            buffer[(15, 1)].style().fg,
            Some(Color::DarkGray),
            "the comment is dim"
        );
    }

    #[test]
    fn a_block_comment_carries_down_to_later_lines() {
        use ratatui::style::Color;

        let mut app = app_named("code.rs", "let a = 1;\n/*\nstill comment\n*/ let y = 2;\n");
        app.set_area(40, 8);
        app.follow_cursor(5);

        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();

        // body rows start at y = 1, so buffer line 2 ("still comment") is row 3
        assert_eq!(buffer[(4, 3)].symbol(), "s");
        assert_eq!(
            buffer[(4, 3)].style().fg,
            Some(Color::DarkGray),
            "the line after `/*` is inside the block comment"
        );
        // the line that closes the comment is comment up to `*/`, then code again
        assert_eq!(buffer[(4, 4)].symbol(), "*");
        assert_eq!(buffer[(4, 4)].style().fg, Some(Color::DarkGray));
        assert_eq!(
            buffer[(4 + 3, 4)].style().fg,
            Some(Color::Magenta),
            "`let` after `*/` is a keyword again"
        );
    }

    #[test]
    fn plain_text_files_get_no_token_colour() {
        use ratatui::style::Color;

        let mut app = app_with("let x = 1;\n");
        app.set_area(40, 8);
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_ne!(
            buffer[(4, 1)].style().fg,
            Some(Color::Magenta),
            "unknown extensions stay unstyled"
        );
    }

    #[test]
    fn the_gutter_shows_a_mark_for_changed_lines() {
        use ratatui::style::Color;
        use std::process::Command;

        let dir = std::env::temp_dir().join(format!("nyavim-ui-git-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .expect("git must be installed");
            assert!(status.success());
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        let path = dir.join("tracked.txt");
        fs::write(&path, "one\ntwo\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "initial"]);

        let mut app = App::new();
        app.set_area(60, 8);
        app.open(&path);
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('A'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        for ch in "-changed".chars() {
            app.handle_key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(ch),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Esc,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        app.follow_cursor(5);

        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();

        // first cell of the body row is the change mark
        assert_eq!(buffer[(0, 1)].symbol(), "~", "changed line is marked");
        assert_eq!(buffer[(0, 1)].style().fg, Some(Color::Yellow));
        assert_eq!(buffer[(0, 2)].symbol(), " ", "unchanged line has no mark");

        let mut status = String::new();
        for x in 0..60 {
            status.push_str(buffer[(x, 6)].symbol());
        }
        assert!(
            status.contains("main"),
            "the branch is in the status line: {status}"
        );
    }
}
