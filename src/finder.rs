//! File discovery for the `Ctrl+P` picker.
//!
//! A bounded, synchronous walk: no daemon, no index, no watcher. Directories that never belong in a
//! picker are skipped, and the number of entries is capped so a runaway tree cannot stall a frame.

use std::path::{Path, PathBuf};

/// Directories that are never worth listing.
const SKIP: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".idea",
    ".venv",
    "__pycache__",
];

/// Files under `root`, as paths relative to it, sorted for stable ordering.
pub fn collect(root: &Path, limit: usize) -> Vec<String> {
    let mut found = Vec::new();
    walk(root, root, &mut found, limit, 0);
    found.sort();
    found
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>, limit: usize, depth: usize) {
    if out.len() >= limit || depth > 12 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|e| e.path())
        .collect();
    children.sort();

    for path in children {
        if out.len() >= limit {
            return;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            if SKIP.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            walk(root, &path, out, limit, depth + 1);
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nyavim-finder-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("src/deep/lib.rs"), "").unwrap();
        std::fs::write(dir.join(".git/config"), "").unwrap();
        std::fs::write(dir.join("target/binary"), "").unwrap();
        dir
    }

    #[test]
    fn collects_files_and_skips_noise_directories() {
        let dir = tree("basic");
        let files = collect(&dir, 100);
        assert!(files.contains(&"README.md".to_owned()));
        assert!(files.contains(&"src/main.rs".to_owned()));
        assert!(files.contains(&"src/deep/lib.rs".to_owned()));
        assert!(!files.iter().any(|f| f.starts_with(".git/")));
        assert!(!files.iter().any(|f| f.starts_with("target/")));
        assert_eq!(files, {
            let mut sorted = files.clone();
            sorted.sort();
            sorted
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_limit_is_respected() {
        let dir = tree("limit");
        assert!(collect(&dir, 2).len() <= 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_root_yields_nothing() {
        assert!(collect(Path::new("/nonexistent/nyavim-finder"), 10).is_empty());
    }
}
