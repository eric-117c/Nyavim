//! Terminal setup, event loop, and teardown. Everything that knows about the tty lives here.

mod app;
mod command;
mod document;
mod mode;
mod ui;

use std::io;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::Terminal;

use app::{App, Outcome};

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

/// Tab bar + status line + hint line.
const HINT_ROWS: u16 = 3;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut app = App::new();
    if let Some(path) = args.first() {
        app.open(path);
    }

    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;

    if let Err(err) = result {
        eprintln!("nyavim: {err}");
        std::process::exit(1);
    }
    Ok(())
}

fn setup_terminal() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // A panic must not leave the user with a wedged terminal.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal_direct();
        previous_hook(info);
    }));

    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

fn restore_terminal_direct() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)
}

fn run(terminal: &mut Tui, app: &mut App) -> io::Result<()> {
    loop {
        let visible = terminal.size()?.height.saturating_sub(HINT_ROWS) as usize;
        app.follow_cursor(visible);
        terminal.draw(|frame| ui::draw(frame, app))?;

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if app.handle_key(key) == Outcome::Quit {
                    return Ok(());
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}
