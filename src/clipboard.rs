//! The system clipboard.
//!
//! Two ways out: a helper program when one is available (the reliable path on a local desktop), and
//! OSC 52 when it is not — that escape sequence asks the *terminal* to set the clipboard, which is
//! what makes yanking work over ssh. Reading only works through a helper: OSC 52 has no read side.
//!
//! `NYAVIM_CLIPBOARD_COPY` / `NYAVIM_CLIPBOARD_PASTE` override both paths with a shell command,
//! which is how the tests exercise this without needing a display server.

use std::io::Write;
use std::process::{Command, Stdio};

/// Tools that set the clipboard, most specific first.
const COPY_TOOLS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("pbcopy", &[]),
];

/// Tools that read it.
const PASTE_TOOLS: &[(&str, &[&str])] = &[
    ("wl-paste", &["--no-newline"]),
    ("xclip", &["-selection", "clipboard", "-o"]),
    ("pbpaste", &[]),
];

fn override_command(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Put `text` on the system clipboard. Returns false when nothing could take it.
pub fn copy(text: &str) -> bool {
    if let Some(command) = override_command("NYAVIM_CLIPBOARD_COPY") {
        return run_shell(&command, Some(text)).is_some();
    }
    for (tool, args) in COPY_TOOLS {
        let child = Command::new(tool)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|status| status.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    osc52(text)
}

/// Read the clipboard, if a helper is available.
pub fn paste() -> Option<String> {
    if let Some(command) = override_command("NYAVIM_CLIPBOARD_PASTE") {
        return run_shell(&command, None).filter(|text| !text.is_empty());
    }
    for (tool, args) in PASTE_TOOLS {
        let output = Command::new(tool)
            .args(*args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout).into_owned();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn run_shell(command: &str, input: Option<&str>) -> Option<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let (Some(text), Some(stdin)) = (input, child.stdin.as_mut()) {
        let _ = stdin.write_all(text.as_bytes());
    }
    // close our end so the command sees end-of-input before we wait for it
    child.stdin.take();
    let output = child.wait_with_output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Ask the terminal itself to set the clipboard.
fn osc52(text: &str) -> bool {
    use base64_lite::encode;
    let mut stdout = std::io::stdout();
    let sequence = format!("\x1b]52;c;{}\x07", encode(text.as_bytes()));
    if stdout.write_all(sequence.as_bytes()).is_err() {
        return false;
    }
    let _ = stdout.flush();
    true
}

/// Minimal base64, so this module needs no dependency of its own.
mod base64_lite {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_lite::encode(b""), "");
        assert_eq!(base64_lite::encode(b"f"), "Zg==");
        assert_eq!(base64_lite::encode(b"fo"), "Zm8=");
        assert_eq!(base64_lite::encode(b"foo"), "Zm9v");
        assert_eq!(base64_lite::encode(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn a_shell_command_round_trips_text_through_a_file() {
        // Exercised through `run_shell` rather than the env override: tests run in one process and
        // sharing an environment variable between parallel tests is a race waiting to happen.
        let dir = std::env::temp_dir().join(format!("nyavim-clip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.txt");

        let write = format!("cat > {}", file.display());
        assert!(run_shell(&write, Some("hello clipboard")).is_some());
        let read = format!("cat {}", file.display());
        assert_eq!(run_shell(&read, None).as_deref(), Some("hello clipboard"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
