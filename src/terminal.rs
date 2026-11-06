//! A real shell on a real pty.
//!
//! The editor hands the whole screen over, forwards raw keystrokes, and renders whatever the shell
//! prints by parsing its escape sequences into a grid. Interactive programs therefore behave: job
//! control, colours, full-screen apps and terminal resizes all come from the pty, not from us.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

pub struct Session {
    master: File,
    child: Child,
    parser: vt100::Parser,
    finished: bool,
}

impl Session {
    /// Start `shell` on a fresh pty sized `rows` × `cols`.
    pub fn spawn(shell: &str, rows: u16, cols: u16) -> io::Result<Self> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let size = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty writes both descriptors on success; the arguments are all plain ints/null.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        };
        if opened != 0 {
            return Err(io::Error::last_os_error());
        }

        // The child needs its own descriptors; the parent keeps the master side only.
        // SAFETY: `slave` is a valid fd from openpty, and each dup is checked below.
        let (stdin_fd, stdout_fd, stderr_fd) = unsafe {
            let a = libc::dup(slave);
            let b = libc::dup(slave);
            let c = libc::dup(slave);
            if a < 0 || b < 0 || c < 0 {
                libc::close(slave);
                libc::close(master);
                return Err(io::Error::last_os_error());
            }
            libc::close(slave);
            (a, b, c)
        };

        let mut command = Command::new(shell);
        command
            .stdin(unsafe { Stdio::from_raw_fd(stdin_fd) })
            .stdout(unsafe { Stdio::from_raw_fd(stdout_fd) })
            .stderr(unsafe { Stdio::from_raw_fd(stderr_fd) })
            .env("TERM", "xterm-256color");
        // SAFETY: the closure only calls async-signal-safe libc functions between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = command.spawn()?;
        // SAFETY: master came from openpty and is owned by this Session from here on.
        let master = unsafe { File::from_raw_fd(master) };
        set_nonblocking(master.as_raw_fd())?;

        Ok(Self {
            master,
            child,
            parser: vt100::Parser::new(rows.max(1), cols.max(1), 0),
            finished: false,
        })
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if self.parser.screen().size() == (rows, cols) {
            return;
        }
        self.parser.set_size(rows, cols);
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: ioctl only reads the winsize struct we just filled in.
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size);
        }
    }

    /// Send raw keystrokes to the shell.
    pub fn write_input(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // A full pty buffer means the shell is busy; dropping input is better than blocking the UI.
        let _ = self.master.write_all(bytes);
    }

    /// Read everything the shell has produced. Returns true when new output arrived.
    pub fn drain_output(&mut self) -> bool {
        let mut buffer = [0u8; 16 * 1024];
        let mut read_any = false;
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => {
                    self.finished = true;
                    break;
                }
                Ok(count) => {
                    self.parser.process(&buffer[..count]);
                    read_any = true;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.finished = true;
                    break;
                }
            }
        }
        read_any
    }

    /// True once the shell has exited or the pty closed.
    pub fn is_finished(&mut self) -> bool {
        if self.finished {
            return true;
        }
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on our own descriptor with plain integer flags.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
