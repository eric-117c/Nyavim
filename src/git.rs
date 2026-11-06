//! Git integration, kept deliberately thin: ask the `git` binary for the branch and for the file's
//! HEAD content, then diff in-process so unsaved edits show up in the gutter too.
//!
//! Nothing here shells out on a timer or watches the filesystem. A refresh happens when the buffer
//! is opened, saved, committed, or when the user asks for one.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One line's worth of "what changed relative to HEAD".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Unchanged,
    Added,
    Modified,
    /// A line was removed from HEAD above this one, like vim-gitgutter's `_`.
    RemovedAbove,
}

/// Per-buffer view of the repository state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitInfo {
    /// Branch name, or a short commit id when detached. `None` outside a repository.
    pub branch: Option<String>,
    /// HEAD content of the file, line by line. `None` when the file is not in HEAD yet.
    pub head_lines: Option<Vec<String>>,
    pub marks: Vec<Mark>,
}

impl GitInfo {
    pub fn in_repo(&self) -> bool {
        self.branch.is_some()
    }
}

/// Ask git for the branch and the file's HEAD content. `path` is the file on disk.
pub fn load(path: Option<&Path>, buffer: &str) -> GitInfo {
    let Some(path) = path else {
        return GitInfo::default();
    };
    let Some(dir) = path.parent() else {
        return GitInfo::default();
    };
    let Some(root) = repo_root(dir) else {
        return GitInfo::default();
    };
    let relative = path
        .strip_prefix(&root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");

    let branch = branch_name(&root);
    let head_lines = head_content(&root, &relative);
    let mut info = GitInfo {
        branch,
        head_lines,
        marks: Vec::new(),
    };
    info.refresh_marks(buffer);
    info
}

impl GitInfo {
    /// Recompute gutter marks against the cached HEAD content. Cheap enough to run after an edit
    /// because it never starts a process.
    pub fn refresh_marks(&mut self, buffer: &str) {
        let buffer_lines: Vec<String> = buffer.lines().map(str::to_owned).collect();
        self.marks = diff_marks(self.head_lines.as_deref(), &buffer_lines);
    }
}

/// Stage tracked changes and commit them. Returns a one-line result for the status line.
pub fn commit(path: Option<&Path>, message: &str) -> Result<String, String> {
    let path = path.ok_or_else(|| "no file to commit".to_owned())?;
    let dir = path.parent().ok_or_else(|| "no directory".to_owned())?;
    let root = repo_root(dir).ok_or_else(|| "not a git repository".to_owned())?;

    let add = run_git(&root, &["add", "-u"])?;
    if !add.status.success() {
        return Err(first_line(&add.stderr, "git add failed"));
    }
    let commit = run_git(&root, &["commit", "-m", message])?;
    if !commit.status.success() {
        return Err(first_line(&commit.stderr, "git commit failed"));
    }
    Ok(first_line(&commit.stdout, "committed"))
}

fn run_git(dir: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|err| format!("could not run git: {err}"))
}

fn first_line(bytes: &[u8], fallback: &str) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| fallback.to_owned())
}

fn repo_root(dir: &Path) -> Option<PathBuf> {
    let output = run_git(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        None
    } else {
        Some(PathBuf::from(text))
    }
}

fn branch_name(root: &Path) -> Option<String> {
    let output = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if name.is_empty() {
        return None;
    }
    if name == "HEAD" {
        // detached: show the short commit id instead of a branch
        let sha = run_git(root, &["rev-parse", "--short", "HEAD"]).ok()?;
        let sha = String::from_utf8_lossy(&sha.stdout).trim().to_owned();
        return (!sha.is_empty()).then_some(sha);
    }
    Some(name)
}

fn head_content(root: &Path, relative: &str) -> Option<Vec<String>> {
    let spec = format!("HEAD:{relative}");
    let output = run_git(root, &["show", &spec]).ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect(),
    )
}

/// Line-level marks for `buffer` relative to `head`.
///
/// `head == None` means the file is new, so every line counts as added. The diff trims the common
/// prefix and suffix first, which is what makes a one-line edit cost nothing on a large file, and
/// gives up (marking the whole middle as modified) rather than allocate a huge table.
pub fn diff_marks(head: Option<&[String]>, buffer: &[String]) -> Vec<Mark> {
    let mut marks = vec![Mark::Unchanged; buffer.len()];
    let Some(head) = head else {
        marks.fill(Mark::Added);
        return marks;
    };

    let prefix = head
        .iter()
        .zip(buffer.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = head.len().min(buffer.len()) - prefix;
    let suffix = head
        .iter()
        .rev()
        .zip(buffer.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();

    let head_mid = &head[prefix..head.len() - suffix];
    let buffer_mid = &buffer[prefix..buffer.len() - suffix];
    if head_mid.is_empty() && buffer_mid.is_empty() {
        return marks;
    }

    let pairs = match lcs_pairs(head_mid, buffer_mid) {
        Some(pairs) => pairs,
        None => {
            // Too big to align cheaply: report the whole region as changed.
            marks[prefix..buffer.len() - suffix].fill(Mark::Modified);
            return marks;
        }
    };

    let mut head_pos = 0;
    let mut buffer_pos = 0;
    for (head_match, buffer_match) in pairs
        .iter()
        .copied()
        .chain(std::iter::once((head_mid.len(), buffer_mid.len())))
    {
        let removed = head_match - head_pos;
        let added = buffer_match - buffer_pos;
        let paired = removed.min(added);
        for offset in 0..paired {
            marks[prefix + buffer_pos + offset] = Mark::Modified;
        }
        for offset in paired..added {
            marks[prefix + buffer_pos + offset] = Mark::Added;
        }
        if removed > paired {
            let at = prefix + buffer_pos + added;
            if at < marks.len() {
                marks[at] = Mark::RemovedAbove;
            } else if let Some(last) = marks.last_mut() {
                if *last == Mark::Unchanged {
                    *last = Mark::RemovedAbove;
                }
            }
        }
        head_pos = head_match + 1;
        buffer_pos = buffer_match + 1;
    }

    marks
}

/// Matching line index pairs via LCS. `None` when the block is too large to align cheaply.
fn lcs_pairs(a: &[String], b: &[String]) -> Option<Vec<(usize, usize)>> {
    const MAX_CELLS: usize = 1_000_000;
    if a.is_empty() || b.is_empty() {
        return Some(Vec::new());
    }
    if a.len().saturating_mul(b.len()) > MAX_CELLS {
        return None;
    }

    let width = b.len() + 1;
    let mut table = vec![0u32; (a.len() + 1) * width];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i * width + j] = if a[i] == b[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }

    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    Some(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    fn marks(head: &str, buffer: &str) -> Vec<Mark> {
        diff_marks(Some(&lines(head)), &lines(buffer))
    }

    #[test]
    fn identical_files_have_no_marks() {
        assert_eq!(marks("a\nb\n", "a\nb\n"), vec![Mark::Unchanged; 2]);
    }

    #[test]
    fn a_new_file_is_all_added() {
        let got = diff_marks(None, &lines("a\nb\n"));
        assert_eq!(got, vec![Mark::Added; 2]);
    }

    #[test]
    fn an_appended_line_is_added() {
        let got = marks("a\nb\n", "a\nb\nc\n");
        assert_eq!(got, vec![Mark::Unchanged, Mark::Unchanged, Mark::Added]);
    }

    #[test]
    fn a_changed_line_is_modified() {
        let got = marks("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(got, vec![Mark::Unchanged, Mark::Modified, Mark::Unchanged]);
    }

    #[test]
    fn a_deleted_line_marks_the_line_below_it() {
        let got = marks("a\nb\nc\n", "a\nc\n");
        assert_eq!(got, vec![Mark::Unchanged, Mark::RemovedAbove]);
    }

    #[test]
    fn deleting_the_last_line_marks_the_one_above() {
        let got = marks("a\nb\nc\n", "a\nb\n");
        assert_eq!(got, vec![Mark::Unchanged, Mark::RemovedAbove]);
    }

    #[test]
    fn two_changes_in_one_run_pair_into_modifications() {
        let got = marks("a\nb\nc\nd\n", "a\nX\nY\nd\n");
        assert_eq!(
            got,
            vec![
                Mark::Unchanged,
                Mark::Modified,
                Mark::Modified,
                Mark::Unchanged
            ]
        );
    }

    #[test]
    fn more_removed_than_added_leaves_a_removed_marker() {
        let got = marks("a\nb\nc\nz\n", "a\nQ\nz\n");
        assert_eq!(
            got,
            vec![Mark::Unchanged, Mark::Modified, Mark::RemovedAbove],
            "one line replaced, two removed"
        );
    }

    #[test]
    fn an_empty_head_marks_everything_added() {
        let got = diff_marks(Some(&[]), &lines("a\nb\n"));
        assert_eq!(got, vec![Mark::Added; 2]);
    }

    #[test]
    fn a_huge_rewrite_degrades_to_modified_instead_of_hanging() {
        let head: Vec<String> = (0..1200).map(|i| format!("line {i}")).collect();
        let buffer: Vec<String> = (0..1200).map(|i| format!("other {i}")).collect();
        let got = diff_marks(Some(&head), &buffer);
        assert_eq!(got.len(), 1200);
        assert!(got.iter().all(|mark| *mark == Mark::Modified));
    }

    #[test]
    fn an_edit_in_the_middle_of_a_big_file_only_touches_its_own_line() {
        let head: Vec<String> = (0..5000).map(|i| format!("line {i}")).collect();
        let mut buffer = head.clone();
        buffer[2500] = "changed".to_owned();
        let got = diff_marks(Some(&head), &buffer);
        assert_eq!(got[2500], Mark::Modified);
        assert_eq!(
            got.iter().filter(|mark| **mark != Mark::Unchanged).count(),
            1
        );
    }
}
