//! User configuration: `~/.config/nyavim/config.toml`.
//!
//! Everything is optional and a broken file must never stop the editor from starting: bad values are
//! collected as warnings shown in the status line, and the defaults stay in place.
//!
//! Key bindings are deliberately *not* configurable yet — the keymap is a hand-written dispatch over
//! `KeyCode`, so rebinding needs a data-driven action table first. The file format reserves nothing
//! for it, so adding it later is not a breaking change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::style::Color;
use serde::Deserialize;

use crate::syntax::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub keyword: Color,
    pub string: Color,
    pub comment: Color,
    pub number: Color,
    pub type_name: Color,
    pub heading: Color,
    pub link: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            keyword: Color::Magenta,
            string: Color::Green,
            comment: Color::DarkGray,
            number: Color::Yellow,
            type_name: Color::Cyan,
            heading: Color::Cyan,
            link: Color::Blue,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub tab_width: usize,
    /// Fold long lines onto the next screen row instead of letting them run off the edge.
    pub wrap: bool,
    /// Lines kept visible above/below the cursor while scrolling.
    pub scroll_margin: usize,
    pub swap_seconds: u64,
    pub read_only_mb: u64,
    pub theme: Theme,
    pub filetypes: HashMap<String, Language>,
    /// Problems found while reading the file, shown once in the status line.
    pub warnings: Vec<String>,
    pub path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tab_width: crate::ui::TAB_WIDTH,
            wrap: true,
            scroll_margin: 2,
            swap_seconds: 2,
            read_only_mb: 50,
            theme: Theme::default(),
            filetypes: HashMap::new(),
            warnings: Vec::new(),
            path: None,
        }
    }
}

impl Config {
    /// Read the user's file, falling back to defaults on any problem.
    pub fn load() -> Self {
        match config_path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    pub fn load_from(path: &Path) -> Self {
        let mut config = Config {
            path: Some(path.to_path_buf()),
            ..Config::default()
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                if err.kind() != std::io::ErrorKind::NotFound {
                    config.warnings.push(format!("{}: {err}", path.display()));
                }
                return config;
            }
        };
        let raw: Raw = match toml::from_str(&text) {
            Ok(raw) => raw,
            Err(err) => {
                config.warnings.push(format!(
                    "{}: {}",
                    path.display(),
                    first_line(&err.to_string())
                ));
                return config;
            }
        };

        if let Some(editor) = raw.editor {
            if let Some(width) = editor.tab_width {
                match width {
                    1..=16 => config.tab_width = width,
                    _ => config
                        .warnings
                        .push("tab_width must be between 1 and 16".to_owned()),
                }
            }
            if let Some(wrap) = editor.wrap {
                config.wrap = wrap;
            }
            if let Some(margin) = editor.scroll_margin {
                config.scroll_margin = margin.min(20);
            }
            if let Some(seconds) = editor.swap_seconds {
                config.swap_seconds = seconds.clamp(1, 60);
            }
            if let Some(mb) = editor.read_only_mb {
                config.read_only_mb = mb.max(1);
            }
        }

        if let Some(theme) = raw.theme {
            for (name, value) in theme {
                let Some(color) = parse_color(&value) else {
                    config.warnings.push(format!("unknown colour: {value}"));
                    continue;
                };
                match name.as_str() {
                    "keyword" => config.theme.keyword = color,
                    "string" => config.theme.string = color,
                    "comment" => config.theme.comment = color,
                    "number" => config.theme.number = color,
                    "type" => config.theme.type_name = color,
                    "heading" => config.theme.heading = color,
                    "link" => config.theme.link = color,
                    other => config.warnings.push(format!("unknown theme key: {other}")),
                }
            }
        }

        if let Some(filetypes) = raw.filetypes {
            for (extension, language) in filetypes {
                match parse_language(&language) {
                    Some(language) => {
                        config
                            .filetypes
                            .insert(extension.trim_start_matches('.').to_lowercase(), language);
                    }
                    None => config
                        .warnings
                        .push(format!("unknown language: {language}")),
                }
            }
        }

        config
    }

    /// Language for a file, honouring `[filetypes]` before the built-in extensions.
    pub fn language_for(&self, path: Option<&Path>) -> Language {
        if let Some(path) = path {
            if let Some(extension) = path.extension() {
                let extension = extension.to_string_lossy().to_lowercase();
                if let Some(language) = self.filetypes.get(&extension) {
                    return *language;
                }
            }
        }
        Language::detect(path)
    }

    pub fn read_only_limit(&self) -> u64 {
        self.read_only_mb.saturating_mul(1024 * 1024)
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("NYAVIM_CONFIG") {
        return Some(PathBuf::from(explicit));
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("nyavim/config.toml"));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config/nyavim/config.toml"))
}

#[derive(Debug, Deserialize, Default)]
struct Raw {
    editor: Option<RawEditor>,
    theme: Option<HashMap<String, String>>,
    filetypes: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize, Default)]
struct RawEditor {
    tab_width: Option<usize>,
    wrap: Option<bool>,
    scroll_margin: Option<usize>,
    swap_seconds: Option<u64>,
    read_only_mb: Option<u64>,
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// ANSI names plus `0`–`255` for terminals with a 256-colour palette.
pub fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim().to_lowercase();
    let named = match value.as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "white" => Some(Color::White),
        "reset" | "default" => Some(Color::Reset),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    value.parse::<u8>().ok().map(Color::Indexed)
}

fn parse_language(value: &str) -> Option<Language> {
    match value.trim().to_lowercase().as_str() {
        "text" | "plain" => Some(Language::Plain),
        "rust" | "rs" => Some(Language::Rust),
        "toml" => Some(Language::Toml),
        "json" => Some(Language::Json),
        "markdown" | "md" => Some(Language::Markdown),
        "shell" | "sh" | "bash" => Some(Language::Shell),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(tag: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nyavim-cfg-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_missing_file_is_just_defaults() {
        let config = Config::load_from(Path::new("/nonexistent/nyavim/config.toml"));
        assert_eq!(config.tab_width, crate::ui::TAB_WIDTH);
        assert_eq!(config.scroll_margin, 2);
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn editor_settings_are_read_and_clamped() {
        let path = write(
            "editor",
            "[editor]\ntab_width = 8\nscroll_margin = 99\nswap_seconds = 0\nread_only_mb = 10\n",
        );
        let config = Config::load_from(&path);
        assert_eq!(config.tab_width, 8);
        assert_eq!(config.scroll_margin, 20, "clamped");
        assert_eq!(config.swap_seconds, 1, "clamped to at least a second");
        assert_eq!(config.read_only_limit(), 10 * 1024 * 1024);
    }

    #[test]
    fn an_out_of_range_tab_width_is_refused_with_a_warning() {
        let path = write("tabwidth", "[editor]\ntab_width = 40\n");
        let config = Config::load_from(&path);
        assert_eq!(config.tab_width, crate::ui::TAB_WIDTH);
        assert!(config.warnings.iter().any(|w| w.contains("tab_width")));
    }

    #[test]
    fn theme_colours_accept_names_and_indices() {
        let path = write(
            "theme",
            "[theme]\nkeyword = \"cyan\"\nstring = \"108\"\ncomment = \"nonsense\"\n",
        );
        let config = Config::load_from(&path);
        assert_eq!(config.theme.keyword, Color::Cyan);
        assert_eq!(config.theme.string, Color::Indexed(108));
        assert!(config.warnings.iter().any(|w| w.contains("nonsense")));
    }

    #[test]
    fn filetypes_override_the_builtin_extensions() {
        let path = write("filetypes", "[filetypes]\npy = \"rust\"\nino = \"nope\"\n");
        let config = Config::load_from(&path);
        assert_eq!(
            config.language_for(Some(Path::new("script.py"))),
            Language::Rust
        );
        assert!(config.warnings.iter().any(|w| w.contains("nope")));
        assert_eq!(
            config.language_for(Some(Path::new("main.rs"))),
            Language::Rust,
            "built-in detection still works"
        );
    }

    #[test]
    fn a_broken_file_warns_instead_of_failing() {
        let path = write("broken", "this is not toml [[[");
        let config = Config::load_from(&path);
        assert!(!config.warnings.is_empty());
        assert_eq!(config.tab_width, crate::ui::TAB_WIDTH);
    }
}
