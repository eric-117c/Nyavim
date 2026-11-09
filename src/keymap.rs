//! The action table: what a key *means*, separated from what the key *is*.
//!
//! This is the first slice of a larger refactor. Motions and operators still live in the hand-written
//! matcher in `app`, because they carry counts and pending state; everything that maps one key to one
//! self-contained action is here. Growing this table is how key rebinding becomes possible later —
//! a config would rewrite the bindings, not the dispatch.

use ratatui::crossterm::event::KeyCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Quit,
    Palette,
    WindowCommand,
    Terminal,
    FindFile,
    NextBuffer,
    PrevBuffer,
    Undo,
    Redo,
    Visual,
    JoinLines,
    MatchBracket,
    SearchForward,
    SearchBackward,
}

/// Actions reachable with `Ctrl` plus one key.
pub fn ctrl_binding(code: KeyCode) -> Option<Action> {
    let action = match code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('k') => Action::Palette,
        KeyCode::Char('w') => Action::WindowCommand,
        KeyCode::Char('t') => Action::Terminal,
        KeyCode::Char('p') => Action::FindFile,
        KeyCode::Char('n') => Action::NextBuffer,
        KeyCode::Char('b') => Action::PrevBuffer,
        KeyCode::Char('r') => Action::Redo,
        _ => return None,
    };
    Some(action)
}

/// Actions reachable with a bare key in normal mode and in visual mode.
pub fn plain_binding(code: KeyCode) -> Option<Action> {
    let action = match code {
        KeyCode::Char('u') => Action::Undo,
        KeyCode::Char('v') => Action::Visual,
        KeyCode::Char('J') => Action::JoinLines,
        KeyCode::Char('%') => Action::MatchBracket,
        KeyCode::Char('/') => Action::SearchForward,
        KeyCode::Char('?') => Action::SearchBackward,
        _ => return None,
    };
    Some(action)
}

/// Every binding, for the help page and for a future config layer.
pub fn bindings() -> Vec<(&'static str, Action)> {
    vec![
        ("Ctrl+Q", Action::Quit),
        ("Ctrl+K", Action::Palette),
        ("Ctrl+W", Action::WindowCommand),
        ("Ctrl+T", Action::Terminal),
        ("Ctrl+P", Action::FindFile),
        ("Ctrl+N", Action::NextBuffer),
        ("Ctrl+B", Action::PrevBuffer),
        ("Ctrl+R", Action::Redo),
        ("u", Action::Undo),
        ("v", Action::Visual),
        ("J", Action::JoinLines),
        ("%", Action::MatchBracket),
        ("/", Action::SearchForward),
        ("?", Action::SearchBackward),
    ]
}

impl Action {
    /// Short label used in the palette and help.
    pub fn name(self) -> &'static str {
        match self {
            Action::Quit => "quit",
            Action::Palette => "palette",
            Action::WindowCommand => "window commands",
            Action::Terminal => "terminal",
            Action::FindFile => "find-file",
            Action::NextBuffer => "next-buffer",
            Action::PrevBuffer => "prev-buffer",
            Action::Undo => "undo",
            Action::Redo => "redo",
            Action::Visual => "visual",
            Action::JoinLines => "join-lines",
            Action::MatchBracket => "match-bracket",
            Action::SearchForward => "search",
            Action::SearchBackward => "search-backward",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_bindings_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for key in ['q', 'k', 'w', 't', 'p', 'n', 'b', 'r', 'z', 'y'] {
            if let Some(action) = ctrl_binding(KeyCode::Char(key)) {
                assert!(seen.insert(action), "Ctrl+{key} collides on {action:?}");
            }
        }
        assert!(ctrl_binding(KeyCode::Char('z')).is_none());
    }

    #[test]
    fn plain_bindings_do_not_shadow_insert_or_operator_keys() {
        for reserved in ['i', 'a', 'o', 'd', 'c', 'y', 'p', 'x'] {
            assert!(
                plain_binding(KeyCode::Char(reserved)).is_none(),
                "{reserved} must stay with the editing matcher"
            );
        }
        assert_eq!(plain_binding(KeyCode::Char('u')), Some(Action::Undo));
    }

    #[test]
    fn the_published_table_matches_the_lookup_functions() {
        for (label, action) in bindings() {
            let found = if let Some(key) = label.strip_prefix("Ctrl+") {
                ctrl_binding(KeyCode::Char(
                    key.chars().next().unwrap().to_ascii_lowercase(),
                ))
            } else {
                plain_binding(KeyCode::Char(label.chars().next().unwrap()))
            };
            assert_eq!(found, Some(action), "{label}");
        }
    }
}
