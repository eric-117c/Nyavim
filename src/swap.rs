//! Swap files: the "did I lose that edit?" safety net.
//!
//! A dirty buffer is written to a hidden sibling file every couple of seconds. Opening a file that
//! has one asks the user whether to recover it. The file is removed on save, on a clean quit, and
//! once the user answers the question — so it never accumulates.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// `.name.nyavim.swp` next to the file it belongs to.
pub fn path_for(file: &Path) -> PathBuf {
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "nyavim".to_owned());
    let swap_name = format!(".{name}.nyavim.swp");
    match file.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(swap_name),
        _ => PathBuf::from(swap_name),
    }
}

pub fn write(file: &Path, text: &str) -> io::Result<()> {
    let swap = path_for(file);
    let tmp = swap.with_extension("swp.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &swap)
}

pub fn read(file: &Path) -> Option<String> {
    let swap = path_for(file);
    fs::read_to_string(swap).ok()
}

pub fn clear(file: &Path) {
    let _ = fs::remove_file(path_for(file));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nyavim-swap-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_swap_file_is_a_hidden_sibling() {
        let swap = path_for(Path::new("/home/me/notes.txt"));
        assert_eq!(swap, PathBuf::from("/home/me/.notes.txt.nyavim.swp"));
    }

    #[test]
    fn write_read_and_clear_round_trip() {
        let dir = dir("roundtrip");
        let file = dir.join("a.txt");
        fs::write(&file, "on disk\n").unwrap();

        assert!(read(&file).is_none());
        write(&file, "unsaved work\n").unwrap();
        assert_eq!(read(&file).as_deref(), Some("unsaved work\n"));

        clear(&file);
        assert!(read(&file).is_none());
        assert_eq!(fs::read_to_string(&file).unwrap(), "on disk\n");
        fs::remove_dir_all(&dir).unwrap();
    }
}
