//! The command registry — the single source of the product's discoverability promise.
//!
//! The palette, the `:` command line, the `F1` help page and the hint line all read from the tables
//! here. Adding an action to this file makes it reachable from all four at once, which is why no
//! feature should implement its own key path around them.

/// What a command is called, what it does, and the keys that reach it without going through the
/// palette.
pub struct Command {
    /// Canonical name, also typed after `:`.
    pub name: &'static str,
    /// Alternative names accepted after `:`.
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    /// Key binding shown next to the command. Actions that only have a key sequence keep it here.
    pub keys: &'static str,
}

pub const COMMANDS: &[Command] = &[
    Command {
        name: "write",
        aliases: &["w", "save"],
        description: "Save the current file; an argument saves as that path",
        keys: ":w [path]",
    },
    Command {
        name: "quit",
        aliases: &["q"],
        description: "Quit, refusing while there are unsaved changes",
        keys: ":q",
    },
    Command {
        name: "quit-force",
        aliases: &["q!"],
        description: "Quit and discard unsaved changes",
        keys: ":q!",
    },
    Command {
        name: "write-quit",
        aliases: &["wq", "x"],
        description: "Save, then quit",
        keys: ":wq",
    },
    Command {
        name: "undo",
        aliases: &["u"],
        description: "Undo the last change",
        keys: "u",
    },
    Command {
        name: "redo",
        aliases: &[],
        description: "Redo the change that was just undone",
        keys: "Ctrl+R",
    },
    Command {
        name: "edit",
        aliases: &["e"],
        description: "Open another file in this buffer",
        keys: ":e <path>",
    },
    Command {
        name: "next-buffer",
        aliases: &["bn"],
        description: "Show the next open file",
        keys: "Ctrl+N",
    },
    Command {
        name: "prev-buffer",
        aliases: &["bp"],
        description: "Show the previous open file",
        keys: "Ctrl+P",
    },
    Command {
        name: "buffer-delete",
        aliases: &["bd"],
        description: "Close this file, refusing while it has unsaved changes",
        keys: ":bd",
    },
    Command {
        name: "help",
        aliases: &[],
        description: "List every command and key binding",
        keys: "F1",
    },
    Command {
        name: "palette",
        aliases: &[],
        description: "Search all commands by name",
        keys: "Ctrl+K",
    },
];

/// Every key that does something without the palette, for the help page and the hint line.
pub const KEYS: &[(&str, &str)] = &[
    ("h j k l", "move by character / line"),
    ("w b", "next / previous word"),
    ("0 $", "start / end of line"),
    ("gg G", "first / last line"),
    ("3 j", "count repeats a motion"),
    ("i a", "insert before / after the cursor"),
    ("I A", "insert at the start / end of the line"),
    ("o O", "open a line below / above"),
    ("Esc", "leave insert mode"),
    ("d c y", "delete / change / yank, then a motion"),
    ("dd cc yy", "the same, for whole lines"),
    ("x", "delete the character under the cursor"),
    ("u Ctrl+R", "undo / redo"),
    ("p P", "paste after / before the cursor"),
    (":", "command line"),
    ("Ctrl+K", "command palette"),
    ("Ctrl+N Ctrl+P", "next / previous open file"),
    ("F1", "this page"),
    ("Ctrl+Q", "quit"),
];

/// Resolve what the user typed after `:` to a command.
pub fn find(word: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|cmd| {
        cmd.name.eq_ignore_ascii_case(word)
            || cmd
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(word))
    })
}

/// Commands matching a palette query, best first. Both the name and the description are searched,
/// so "save" finds `write` — the whole point of a searchable palette is that the user does not have
/// to know our vocabulary.
pub fn search(query: &str) -> Vec<&'static Command> {
    let mut scored: Vec<(i32, &'static Command)> = COMMANDS
        .iter()
        .filter_map(|cmd| {
            let by_name = fuzzy_score(query, cmd.name);
            let by_description = fuzzy_score(query, cmd.description).map(|s| s / 2);
            let score = match (by_name, by_description) {
                (Some(a), Some(b)) => Some(a.max(b) ),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }?;
            Some((score, cmd))
        })
        .collect();
    // Stable ordering: score first, then registry order for ties.
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, cmd)| cmd).collect()
}

/// Subsequence match with a bias towards consecutive and leading matches. `None` when the query
/// does not appear in order.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let mut score = 0;
    let mut query_chars = query.chars().peekable();
    let mut last_match: Option<usize> = None;

    for (idx, cand) in candidate.chars().enumerate() {
        let Some(q) = query_chars.peek() else { break };
        if !cand.eq_ignore_ascii_case(q) {
            continue;
        }
        query_chars.next();
        score += match last_match {
            Some(prev) if prev + 1 == idx => 10,
            _ => 1,
        };
        if idx == 0 {
            score += 20;
        }
        last_match = Some(idx);
    }

    if query_chars.peek().is_some() {
        None
    } else {
        Some(score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_to_the_canonical_command() {
        assert_eq!(find("w").unwrap().name, "write");
        assert_eq!(find("W").unwrap().name, "write");
        assert_eq!(find("q!").unwrap().name, "quit-force");
        assert!(find("nonsense").is_none());
    }

    #[test]
    fn search_finds_commands_by_what_they_do() {
        let hits = search("save");
        assert!(
            hits.iter().any(|cmd| cmd.name == "write"),
            "searching 'save' must find the write command"
        );
    }

    #[test]
    fn search_ranks_name_matches_first() {
        let hits = search("quit");
        assert_eq!(hits[0].name, "quit");
    }

    #[test]
    fn empty_query_lists_everything() {
        assert_eq!(search("").len(), COMMANDS.len());
    }

    #[test]
    fn fuzzy_requires_an_in_order_subsequence() {
        assert!(fuzzy_score("wr", "write").is_some());
        assert!(fuzzy_score("tr", "write").is_none());
        assert!(fuzzy_score("", "write").is_some());
    }

    #[test]
    fn consecutive_matches_outscore_scattered_ones() {
        let tight = fuzzy_score("writ", "write").unwrap();
        let loose = fuzzy_score("writ", "w r i t e").unwrap();
        assert!(tight > loose);
    }
}
