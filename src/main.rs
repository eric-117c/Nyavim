//! Terminal setup, event loop, and teardown. Everything that knows about the tty lives here.

mod app;
mod command;
mod document;
mod git;
mod layout;
mod mode;
mod syntax;
mod terminal;
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

/// Hand the screen to the shell and shuttle bytes until the user comes back.
///
/// Input is read as raw bytes rather than as key events, so the shell sees exactly what the terminal
/// would have sent — arrows, function keys and all. `Ctrl+\\` is the way back.
fn run_terminal(terminal: &mut Tui, app: &mut App) -> io::Result<()> {
    const BACK_TO_EDITOR: u8 = 0x1c;

    loop {
        let size = terminal.size()?;
        app.set_area(size.width, size.height);
        let rows = size.height.saturating_sub(1).max(1);
        let cols = size.width.max(1);

        let (master, finished) = {
            let Some(session) = app.terminal_mut() else {
                app.leave_terminal(false);
                return Ok(());
            };
            session.resize(rows, cols);
            session.drain_output();
            (session.as_raw_fd(), session.is_finished())
        };
        if finished {
            app.leave_terminal(true);
            terminal.draw(|frame| ui::draw(frame, app))?;
            return Ok(());
        }

        terminal.draw(|frame| ui::draw(frame, app))?;

        let mut fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: master,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: poll only reads/writes the revents fields of our two descriptors.
        unsafe {
            libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 60);
        }

        if fds[0].revents & libc::POLLIN != 0 {
            let mut input = [0u8; 4096];
            // SAFETY: reading into a stack buffer we own.
            let count =
                unsafe { libc::read(libc::STDIN_FILENO, input.as_mut_ptr().cast(), input.len()) };
            if count > 0 {
                let bytes = &input[..count as usize];
                match bytes.iter().position(|byte| *byte == BACK_TO_EDITOR) {
                    Some(at) => {
                        if let Some(session) = app.terminal_mut() {
                            session.write_input(&bytes[..at]);
                        }
                        app.leave_terminal(false);
                        return Ok(());
                    }
                    None => {
                        if let Some(session) = app.terminal_mut() {
                            session.write_input(bytes);
                        }
                    }
                }
            }
        }
    }
}

fn run(terminal: &mut Tui, app: &mut App) -> io::Result<()> {
    loop {
        if app.mode() == mode::Mode::Terminal {
            run_terminal(terminal, app)?;
            continue;
        }

        let size = terminal.size()?;
        app.set_area(size.width, size.height);
        let focus = app.focused_window();
        let visible = app
            .window_rects()
            .iter()
            .find(|(id, _)| *id == focus)
            .map(|(_, rect)| rect.height as usize)
            .unwrap_or(size.height.saturating_sub(HINT_ROWS) as usize);
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
