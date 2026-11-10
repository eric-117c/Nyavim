//! The action table: what a key *means*, separated from what the key *is*.
//!
//! Motions, operators and insert entries are all plain data, so the table can be rewritten by the
//! user's `[keys]` config, listed in the help page, and replayed for `.`.

use ratatui::crossterm::event::KeyCode;

/// A cursor movement. Motions are pure: given a cursor they return a new one, so the same table
/// serves plain movement and an operator's range (`d` + `w`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordBackward,
    WordEnd,
    WordEndBig,
    LineStart,
    LineEnd,
}

impl Motion {
    /// `$` includes the character it lands on; the others stop before it, which is what `d$` versus
    /// `dl` needs.
    pub fn inclusive(self) -> bool {
        matches!(self, Motion::LineEnd | Motion::WordEnd | Motion::WordEndBig)
    }
}

/// An operator waiting for its motion or text object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operator {
    Delete,
    Change,
    Yank,
}

/// How an insert session is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InsertKind {
    Here,
    After,
    LineStart,
    LineEnd,
    LineBelow,
    LineAbove,
}

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
    Operator(Operator),
    Insert(InsertKind),
    DeleteChar,
    Paste(bool),
    /// `.` — replay the last change.
    Repeat,
    Move(Motion),
}

impl Action {
    /// Short label used in the help page and by `[keys]`.
    pub fn name(self) -> &'static str {
        match self {
            Action::Quit => "quit",
            Action::Palette => "palette",
            Action::WindowCommand => "window",
            Action::Terminal => "terminal",
            Action::FindFile => "find-file",
            Action::NextBuffer => "next-buffer",
            Action::PrevBuffer => "prev-buffer",
            Action::Undo => "undo",
            Action::Redo => "redo",
            Action::Visual => "visual",
            Action::JoinLines => "join",
            Action::MatchBracket => "match-bracket",
            Action::SearchForward => "search",
            Action::SearchBackward => "search-backward",
            Action::Operator(Operator::Delete) => "delete",
            Action::Operator(Operator::Change) => "change",
            Action::Operator(Operator::Yank) => "yank",
            Action::Insert(InsertKind::Here) => "insert",
            Action::Insert(InsertKind::After) => "append",
            Action::Insert(InsertKind::LineStart) => "insert-line-start",
            Action::Insert(InsertKind::LineEnd) => "append-line-end",
            Action::Insert(InsertKind::LineBelow) => "open-below",
            Action::Insert(InsertKind::LineAbove) => "open-above",
            Action::DeleteChar => "delete-char",
            Action::Paste(false) => "paste",
            Action::Paste(true) => "paste-before",
            Action::Repeat => "repeat",
            Action::Move(Motion::Left) => "motion-left",
            Action::Move(Motion::Right) => "motion-right",
            Action::Move(Motion::Up) => "motion-up",
            Action::Move(Motion::Down) => "motion-down",
            Action::Move(Motion::WordForward) => "motion-word-forward",
            Action::Move(Motion::WordBackward) => "motion-word-backward",
            Action::Move(Motion::WordEnd) => "motion-word-end",
            Action::Move(Motion::WordEndBig) => "motion-word-end-big",
            Action::Move(Motion::LineStart) => "motion-line-start",
            Action::Move(Motion::LineEnd) => "motion-line-end",
        }
    }

    /// Parse the name used in `[keys]` and in the help page.
    pub fn from_name(name: &str) -> Option<Action> {
        let action = match name.trim().to_lowercase().as_str() {
            "quit" => Action::Quit,
            "palette" => Action::Palette,
            "window" | "window-commands" => Action::WindowCommand,
            "terminal" => Action::Terminal,
            "find-file" => Action::FindFile,
            "next-buffer" => Action::NextBuffer,
            "prev-buffer" => Action::PrevBuffer,
            "undo" => Action::Undo,
            "redo" => Action::Redo,
            "visual" => Action::Visual,
            "join" | "join-lines" => Action::JoinLines,
            "match-bracket" => Action::MatchBracket,
            "search" => Action::SearchForward,
            "search-backward" => Action::SearchBackward,
            "delete" => Action::Operator(Operator::Delete),
            "change" => Action::Operator(Operator::Change),
            "yank" => Action::Operator(Operator::Yank),
            "insert" => Action::Insert(InsertKind::Here),
            "append" => Action::Insert(InsertKind::After),
            "insert-line-start" => Action::Insert(InsertKind::LineStart),
            "append-line-end" => Action::Insert(InsertKind::LineEnd),
            "open-below" => Action::Insert(InsertKind::LineBelow),
            "open-above" => Action::Insert(InsertKind::LineAbove),
            "delete-char" => Action::DeleteChar,
            "paste" => Action::Paste(false),
            "paste-before" => Action::Paste(true),
            "repeat" => Action::Repeat,
            "motion-left" => Action::Move(Motion::Left),
            "motion-right" => Action::Move(Motion::Right),
            "motion-up" => Action::Move(Motion::Up),
            "motion-down" => Action::Move(Motion::Down),
            "motion-word-forward" => Action::Move(Motion::WordForward),
            "motion-word-backward" => Action::Move(Motion::WordBackward),
            "motion-word-end" => Action::Move(Motion::WordEnd),
            "motion-word-end-big" => Action::Move(Motion::WordEndBig),
            "motion-line-start" => Action::Move(Motion::LineStart),
            "motion-line-end" => Action::Move(Motion::LineEnd),
            _ => return None,
        };
        Some(action)
    }
}

/// A key from the config file: which character, and whether `Ctrl` is part of it.
pub fn parse_key(spec: &str) -> Option<(KeyCode, bool)> {
    let spec = spec.trim();
    let lowered = spec.to_lowercase();
    if let Some(rest) = lowered.strip_prefix("ctrl+") {
        let ch = rest.trim().chars().next()?;
        return Some((KeyCode::Char(ch), true));
    }
    let ch = spec.chars().next()?;
    Some((KeyCode::Char(ch), false))
}

/// The live binding table: defaults, optionally rewritten by `[keys]`.
#[derive(Debug, Clone)]
pub struct Bindings {
    plain: Vec<(char, Action)>,
    ctrl: Vec<(char, Action)>,
    warnings: Vec<String>,
}

impl Default for Bindings {
    fn default() -> Self {
        let mut bindings = Self {
            plain: Vec::new(),
            ctrl: Vec::new(),
            warnings: Vec::new(),
        };
        for (label, action) in defaults() {
            if let Some((KeyCode::Char(ch), ctrl)) = parse_key(label) {
                bindings.insert(ch, ctrl, action);
            }
        }
        bindings
    }
}

/// The built-in table. Keeping it beside `Bindings` means a default cannot be written twice.
fn defaults() -> Vec<(&'static str, Action)> {
    vec![
        ("Ctrl+Q", Action::Quit),
        ("Ctrl+K", Action::Palette),
        ("Ctrl+W", Action::WindowCommand),
        ("Ctrl+T", Action::Terminal),
        ("Ctrl+P", Action::FindFile),
        ("Ctrl+N", Action::NextBuffer),
        ("Ctrl+B", Action::PrevBuffer),
        ("Ctrl+R", Action::Redo),
        ("h", Action::Move(Motion::Left)),
        ("l", Action::Move(Motion::Right)),
        ("j", Action::Move(Motion::Down)),
        ("k", Action::Move(Motion::Up)),
        ("w", Action::Move(Motion::WordForward)),
        ("b", Action::Move(Motion::WordBackward)),
        ("e", Action::Move(Motion::WordEnd)),
        ("E", Action::Move(Motion::WordEndBig)),
        ("0", Action::Move(Motion::LineStart)),
        ("$", Action::Move(Motion::LineEnd)),
        ("u", Action::Undo),
        ("v", Action::Visual),
        ("J", Action::JoinLines),
        ("%", Action::MatchBracket),
        ("/", Action::SearchForward),
        ("?", Action::SearchBackward),
        ("d", Action::Operator(Operator::Delete)),
        ("c", Action::Operator(Operator::Change)),
        ("y", Action::Operator(Operator::Yank)),
        ("i", Action::Insert(InsertKind::Here)),
        ("a", Action::Insert(InsertKind::After)),
        ("I", Action::Insert(InsertKind::LineStart)),
        ("A", Action::Insert(InsertKind::LineEnd)),
        ("o", Action::Insert(InsertKind::LineBelow)),
        ("O", Action::Insert(InsertKind::LineAbove)),
        ("x", Action::DeleteChar),
        ("p", Action::Paste(false)),
        ("P", Action::Paste(true)),
        (".", Action::Repeat),
    ]
}

impl Bindings {
    fn insert(&mut self, ch: char, ctrl: bool, action: Action) {
        let list = if ctrl {
            &mut self.ctrl
        } else {
            &mut self.plain
        };
        match list.iter_mut().find(|(key, _)| *key == ch) {
            Some(slot) => slot.1 = action,
            None => list.push((ch, action)),
        }
    }

    /// Apply `[keys]` from the config: `"Ctrl+E" = "palette"`, `"Q" = "quit"`.
    pub fn apply_overrides(&mut self, overrides: &[(String, String)]) {
        for (label, name) in overrides {
            let Some((KeyCode::Char(ch), ctrl)) = parse_key(label) else {
                self.warnings.push(format!("bad key: {label}"));
                continue;
            };
            let Some(action) = Action::from_name(name) else {
                self.warnings.push(format!("unknown action: {name}"));
                continue;
            };
            self.insert(ch, ctrl, action);
        }
    }

    /// The motion a key is bound to, if any. Operators use this to resolve their argument.
    pub fn motion_for_key(&self, code: KeyCode, ctrl: bool) -> Option<Motion> {
        match self.action_for(code, ctrl) {
            Some(Action::Move(motion)) => Some(motion),
            _ => None,
        }
    }

    /// The key that currently produces `motion`, so `.` can replay an operator through the same
    /// range calculation even after the user rebinds motions.
    pub fn key_for_motion(&self, motion: Motion) -> Option<KeyCode> {
        let plain = self
            .plain
            .iter()
            .find(|(_, action)| *action == Action::Move(motion));
        if let Some((ch, _)) = plain {
            return Some(KeyCode::Char(*ch));
        }
        None
    }

    pub fn action_for(&self, code: KeyCode, ctrl: bool) -> Option<Action> {
        let KeyCode::Char(ch) = code else {
            return None;
        };
        let list = if ctrl { &self.ctrl } else { &self.plain };
        list.iter()
            .find(|(bound, _)| *bound == ch)
            .map(|(_, action)| *action)
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Every binding as `(label, action)`, for the help page.
    pub fn entries(&self) -> Vec<(String, Action)> {
        let mut out: Vec<(String, Action)> = self
            .plain
            .iter()
            .map(|(ch, action)| (ch.to_string(), *action))
            .collect();
        out.extend(
            self.ctrl
                .iter()
                .map(|(ch, action)| (format!("Ctrl+{}", ch.to_ascii_uppercase()), *action)),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_motion_key_is_unique_and_reachable() {
        let bindings = Bindings::default();
        let mut seen = std::collections::HashSet::new();
        for key in ['h', 'l', 'j', 'k', 'w', 'b', 'e', 'E', '0', '$'] {
            let motion = bindings
                .motion_for_key(KeyCode::Char(key), false)
                .unwrap_or_else(|| panic!("{key} is not bound to a motion"));
            assert!(seen.insert(motion), "{key} duplicates {motion:?}");
            assert_eq!(
                bindings.key_for_motion(motion),
                Some(KeyCode::Char(key)),
                "the reverse map must agree"
            );
        }
        assert!(bindings.motion_for_key(KeyCode::Char('z'), false).is_none());
        assert!(Motion::LineEnd.inclusive());
        assert!(!Motion::WordForward.inclusive());
    }

    #[test]
    fn motions_can_be_rebound_like_any_other_action() {
        let mut bindings = Bindings::default();
        bindings.apply_overrides(&[("L".to_owned(), "motion-word-forward".to_owned())]);
        assert_eq!(
            bindings.motion_for_key(KeyCode::Char('L'), false),
            Some(Motion::WordForward)
        );
        let replay_key = bindings
            .key_for_motion(Motion::WordForward)
            .expect("some key still produces the motion");
        assert_eq!(
            bindings.motion_for_key(replay_key, false),
            Some(Motion::WordForward),
            "the replay key resolves back to the same motion"
        );
    }

    #[test]
    fn keys_parse_into_control_and_plain_bindings() {
        assert_eq!(parse_key("Ctrl+K"), Some((KeyCode::Char('k'), true)));
        assert_eq!(parse_key("ctrl+k"), Some((KeyCode::Char('k'), true)));
        assert_eq!(parse_key("J"), Some((KeyCode::Char('J'), false)));
        assert_eq!(parse_key(""), None);
    }

    #[test]
    fn action_names_round_trip() {
        for (_, action) in defaults() {
            assert_eq!(
                Action::from_name(action.name()),
                Some(action),
                "{} does not round-trip",
                action.name()
            );
        }
    }

    #[test]
    fn the_default_table_binds_the_documented_keys() {
        let bindings = Bindings::default();
        assert_eq!(
            bindings.action_for(KeyCode::Char('k'), true),
            Some(Action::Palette)
        );
        assert_eq!(
            bindings.action_for(KeyCode::Char('d'), false),
            Some(Action::Operator(Operator::Delete))
        );
        assert_eq!(
            bindings.action_for(KeyCode::Char('o'), false),
            Some(Action::Insert(InsertKind::LineBelow))
        );
    }

    #[test]
    fn config_overrides_replace_and_add_bindings() {
        let mut bindings = Bindings::default();
        bindings.apply_overrides(&[
            ("Ctrl+E".to_owned(), "palette".to_owned()),
            ("Q".to_owned(), "quit".to_owned()),
        ]);
        assert_eq!(
            bindings.action_for(KeyCode::Char('e'), true),
            Some(Action::Palette),
            "new keys can be added"
        );
        assert_eq!(
            bindings.action_for(KeyCode::Char('Q'), false),
            Some(Action::Quit)
        );
        assert_eq!(
            bindings.action_for(KeyCode::Char('k'), true),
            Some(Action::Palette),
            "untouched defaults stay"
        );
    }

    #[test]
    fn bad_overrides_become_warnings_not_panics() {
        let mut bindings = Bindings::default();
        bindings.apply_overrides(&[
            ("".to_owned(), "palette".to_owned()),
            ("Ctrl+Z".to_owned(), "nonsense".to_owned()),
        ]);
        assert_eq!(bindings.warnings().len(), 2);
        assert_eq!(
            bindings.action_for(KeyCode::Char('z'), true),
            None,
            "a rejected override binds nothing"
        );
    }
}
