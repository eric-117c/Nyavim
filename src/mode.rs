//! Editing modes. Each mode owns its own key interpretation; `App` routes keys by mode.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// `:` command line at the bottom of the screen.
    Command,
    /// `Ctrl+K` command palette.
    Palette,
    /// `F1` key binding reference.
    Help,
    /// Incremental `/` search prompt.
    Search,
    /// `:%s/…/c` asking about one match at a time.
    ReplaceConfirm,
}

impl Mode {
    /// Text shown in the status line and in help.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Command => "COMMAND",
            Mode::Palette => "PALETTE",
            Mode::Help => "HELP",
            Mode::Search => "SEARCH",
            Mode::ReplaceConfirm => "REPLACE",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Mode;

    #[test]
    fn every_mode_has_a_distinct_label() {
        let labels = [
            Mode::Normal.label(),
            Mode::Insert.label(),
            Mode::Command.label(),
            Mode::Palette.label(),
            Mode::Help.label(),
            Mode::Search.label(),
            Mode::ReplaceConfirm.label(),
        ];
        for (i, a) in labels.iter().enumerate() {
            for b in &labels[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
