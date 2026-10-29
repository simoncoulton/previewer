// SPDX-License-Identifier: MIT

//! User preferences, read from `~/.config/previewer/settings.yml`.

use std::{fs, io, path::PathBuf};

use gtk::glib;
use serde::{Deserialize, Serialize};

pub const SYSTEM_THEME_ID: &str = "system";
pub const DEFAULT_THEME_ID: &str = "catppuccin-mocha";

const SETTINGS_FILE: &str = "settings.yml";
const THEMES_DIR: &str = "themes";
const USER_STYLE_FILE: &str = "style.css";

const DEFAULT_SETTINGS: &str = "\
# Previewer settings. Changes are picked up live; no restart needed.

# \"system\" follows the colours your desktop publishes in ~/.config/gtk-4.0
# (Noctalia, matugen, ...). Otherwise use a built-in theme id such as
# \"catppuccin-mocha\", or the file name of a theme in ~/.config/previewer/themes/.
theme: system

# 0 = square corners, 30 = default, 100 = very round.
corner_roundness: 30

# Interface text size in pixels.
text_size: 13

# Interface font family. Leave unset to use the system font.
# interface_font: \"JetBrainsMono Nerd Font\"

# Mouse wheel zooms instead of panning (Ctrl + wheel always zooms).
scroll_zooms: false

# Draw a checkerboard behind transparent images.
checkerboard: true

# Show the thumbnail sidebar when a window opens.
show_sidebar: true

# Turn off interface animations.
reduce_motion: false
";

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub theme: String,
    pub corner_roundness: u32,
    pub text_size: u32,
    pub interface_font: Option<String>,
    pub scroll_zooms: bool,
    pub checkerboard: bool,
    pub show_sidebar: bool,
    pub reduce_motion: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: SYSTEM_THEME_ID.to_owned(),
            corner_roundness: 30,
            text_size: 13,
            interface_font: None,
            scroll_zooms: false,
            checkerboard: true,
            show_sidebar: true,
            reduce_motion: false,
        }
    }
}

impl Settings {
    /// Reads the settings file. A missing file gives the defaults; a broken one
    /// is reported on stderr and also gives the defaults, so a typo never
    /// stops the app from starting.
    pub fn load() -> Self {
        let path = settings_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                eprintln!("previewer: cannot read {}: {error}", path.display());
                return Self::default();
            }
        };
        match parse(&text) {
            Ok(settings) => settings.sanitised(),
            Err(error) => {
                eprintln!("previewer: invalid {}: {error}", path.display());
                Self::default()
            }
        }
    }

    fn sanitised(mut self) -> Self {
        self.corner_roundness = self.corner_roundness.min(100);
        self.text_size = self.text_size.clamp(8, 32);
        self.interface_font = self
            .interface_font
            .take()
            .map(|font| font.trim().to_owned())
            .filter(|font| !font.is_empty());
        self
    }
}

/// A file holding only comments is valid and means "all defaults".
fn parse(text: &str) -> Result<Settings, serde_yaml_ng::Error> {
    let blank = text
        .lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'));
    if blank {
        Ok(Settings::default())
    } else {
        serde_yaml_ng::from_str(text)
    }
}

/// Writes the commented default settings file if the user has none yet.
pub fn ensure_default_settings() {
    let path = settings_path();
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        eprintln!("previewer: cannot create {}: {error}", parent.display());
        return;
    }
    if let Err(error) = fs::write(&path, DEFAULT_SETTINGS) {
        eprintln!("previewer: cannot write {}: {error}", path.display());
    }
}

/// One setting and the value to store for it.
#[derive(Clone, Copy, Debug)]
pub enum Setting<'a> {
    Theme(&'a str),
    CornerRoundness(u32),
    TextSize(u32),
    /// `None` goes back to the system font.
    InterfaceFont(Option<&'a str>),
    ScrollZooms(bool),
    Checkerboard(bool),
    ShowSidebar(bool),
    ReduceMotion(bool),
}

impl Setting<'_> {
    /// The settings key and its YAML value; `None` means the key is removed.
    fn entry(&self) -> (&'static str, Option<String>) {
        match *self {
            Self::Theme(id) => ("theme", Some(yaml_string(id))),
            Self::CornerRoundness(value) => ("corner_roundness", Some(value.to_string())),
            Self::TextSize(value) => ("text_size", Some(value.to_string())),
            Self::InterfaceFont(font) => ("interface_font", font.map(yaml_string)),
            Self::ScrollZooms(value) => ("scroll_zooms", Some(value.to_string())),
            Self::Checkerboard(value) => ("checkerboard", Some(value.to_string())),
            Self::ShowSidebar(value) => ("show_sidebar", Some(value.to_string())),
            Self::ReduceMotion(value) => ("reduce_motion", Some(value.to_string())),
        }
    }
}

/// Persists one setting, keeping the rest of the file (and its comments) as
/// the user left it.
pub fn save(setting: Setting) -> io::Result<()> {
    let path = settings_path();
    let existing = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => DEFAULT_SETTINGS.to_owned(),
        Err(error) => return Err(error),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let (key, value) = setting.entry();
    fs::write(&path, with_key(&existing, key, value.as_deref()))
}

/// `text` with the top-level `key` set to the YAML `value`, or removed when
/// `value` is `None`. A missing key is appended.
fn with_key(text: &str, key: &str, value: Option<&str>) -> String {
    let mut replaced = false;
    let mut out: Vec<String> = Vec::new();
    for existing in text.lines() {
        // Only an unindented `key:` is the setting; comments and keys nested
        // under something else are left alone.
        let is_key = existing
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with(':'));
        if !replaced && is_key {
            replaced = true;
            if let Some(value) = value {
                out.push(format!("{key}: {value}"));
            }
        } else {
            out.push(existing.to_owned());
        }
    }
    if !replaced && let Some(value) = value {
        out.push(format!("{key}: {value}"));
    }
    let mut joined = out.join("\n");
    if !joined.is_empty() {
        joined.push('\n');
    }
    joined
}

/// A double-quoted YAML scalar. Newlines and control characters are escaped
/// so an odd theme id can never break out of its line.
fn yaml_string(value: &str) -> String {
    let mut quoted = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            c if c.is_control() => quoted.push_str(&format!("\\u{:04x}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

pub fn config_dir() -> PathBuf {
    glib::user_config_dir().join("previewer")
}

pub fn settings_path() -> PathBuf {
    config_dir().join(SETTINGS_FILE)
}

pub fn themes_dir() -> PathBuf {
    config_dir().join(THEMES_DIR)
}

pub fn user_style_path() -> PathBuf {
    config_dir().join(USER_STYLE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_file_parses_to_defaults() {
        assert_eq!(parse(DEFAULT_SETTINGS).unwrap(), Settings::default());
    }

    #[test]
    fn partial_file_fills_in_defaults() {
        let parsed = parse("theme: nord").unwrap();
        assert_eq!(parsed.theme, "nord");
        assert_eq!(parsed.text_size, 13);
    }

    #[test]
    fn empty_and_comment_only_files_mean_defaults() {
        assert_eq!(parse("").unwrap(), Settings::default());
        assert_eq!(parse("# nothing\n\n").unwrap(), Settings::default());
    }

    #[test]
    fn wrong_types_are_errors() {
        assert!(parse("text_size: large").is_err());
    }

    #[test]
    fn sanitising_clamps_values() {
        let settings = Settings {
            corner_roundness: 900,
            text_size: 2,
            interface_font: Some("   ".into()),
            ..Settings::default()
        }
        .sanitised();
        assert_eq!(settings.corner_roundness, 100);
        assert_eq!(settings.text_size, 8);
        assert_eq!(settings.interface_font, None);
    }

    #[test]
    fn theme_line_is_replaced_in_place() {
        let text = "# hi\ntheme: system\ntext_size: 14\n";
        let out = with_key(text, "theme", Some("\"nord\""));
        assert_eq!(out, "# hi\ntheme: \"nord\"\ntext_size: 14\n");
    }

    #[test]
    fn theme_line_is_appended_when_missing() {
        let out = with_key("text_size: 14\n", "theme", Some("\"nord\""));
        assert_eq!(out, "text_size: 14\ntheme: \"nord\"\n");
        assert_eq!(parse(&out).unwrap().theme, "nord");
    }

    #[test]
    fn commented_and_indented_theme_lines_are_not_replaced() {
        let out = with_key(
            "# theme: x\nother:\n  theme: y\n",
            "theme",
            Some("\"nord\""),
        );
        assert_eq!(out, "# theme: x\nother:\n  theme: y\ntheme: \"nord\"\n");
    }

    #[test]
    fn numbers_and_flags_round_trip() {
        let mut text = DEFAULT_SETTINGS.to_owned();
        for setting in [
            Setting::CornerRoundness(70),
            Setting::TextSize(16),
            Setting::ScrollZooms(true),
            Setting::Checkerboard(false),
            Setting::ShowSidebar(false),
            Setting::ReduceMotion(true),
            Setting::InterfaceFont(Some("Fira \"Code\"")),
        ] {
            let (key, value) = setting.entry();
            text = with_key(&text, key, value.as_deref());
        }
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.corner_roundness, 70);
        assert_eq!(parsed.text_size, 16);
        assert!(parsed.scroll_zooms && parsed.reduce_motion);
        assert!(!parsed.checkerboard && !parsed.show_sidebar);
        assert_eq!(parsed.interface_font.as_deref(), Some("Fira \"Code\""));
        // Comments from the default file survive.
        assert!(text.contains("# Turn off interface animations."));
    }

    #[test]
    fn clearing_a_setting_removes_only_its_line() {
        let text = "theme: system\ninterface_font: \"Fira\"\ntext_size: 14\n";
        let out = with_key(text, "interface_font", None);
        assert_eq!(out, "theme: system\ntext_size: 14\n");
        assert_eq!(with_key(&out, "interface_font", None), out);
    }

    #[test]
    fn theme_ids_are_escaped() {
        for id in ["we\"ird", "back\\slash", "new\nline: x", "#hash"] {
            let out = with_key("", "theme", Setting::Theme(id).entry().1.as_deref());
            assert_eq!(parse(&out).unwrap().theme, id, "{out}");
        }
    }
}
