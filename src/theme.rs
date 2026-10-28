// SPDX-License-Identifier: MIT

//! Theming.
//!
//! The whole interface is styled from nine colour tokens (see `data/themes.yml`),
//! exposed to CSS as `@pv_bg`, `@pv_surface`, `@pv_text`, `@pv_accent`,
//! `@pv_danger`, `@pv_muted`, `@pv_highlight`, `@pv_border` and `@pv_dim_text`.
//!
//! Layers, lowest priority first:
//!   1. the static stylesheet (`style.css` in the source tree)
//!   2. the generated token layer: colours, font, corner roundness
//!   3. the user's `~/.config/previewer/style.css`, which has the final word
//!
//! All three sit above GTK's own theme and `~/.config/gtk-4.0/gtk.css`.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gtk::{gdk, gio, glib, prelude::*};
use serde::Deserialize;

use crate::settings::{self, DEFAULT_THEME_ID, SYSTEM_THEME_ID, Settings};

const STYLE_CSS: &str = include_str!("style.css");
const CATALOG: &str = include_str!("../data/themes.yml");

const STYLE_PRIORITY: u32 = gtk::STYLE_PROVIDER_PRIORITY_USER + 1;
const TOKEN_PRIORITY: u32 = STYLE_PRIORITY + 1;
const USER_STYLE_PRIORITY: u32 = TOKEN_PRIORITY + 1;

const RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);
const MAX_IMPORT_DEPTH: usize = 4;
const MAX_REFERENCE_DEPTH: usize = 8;

// ---------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const BLACK: Rgb = Rgb(0, 0, 0);

    /// Accepts `#rgb`, `#rrggbb` and `#rrggbbaa` (the alpha is ignored).
    pub fn parse(text: &str) -> Option<Rgb> {
        let digits = text.trim().strip_prefix('#')?;
        if !digits.is_ascii() {
            return None;
        }
        let pair = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
        let single = |i: usize| {
            u8::from_str_radix(&digits[i..i + 1], 16)
                .ok()
                .map(|v| v * 17)
        };
        match digits.len() {
            3 => Some(Rgb(single(0)?, single(1)?, single(2)?)),
            6 | 8 => Some(Rgb(pair(0)?, pair(2)?, pair(4)?)),
            _ => None,
        }
    }

    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// Linear blend: `amount` of `other`, the rest of `self`.
    pub fn mix(self, other: Rgb, amount: f64) -> Rgb {
        let amount = amount.clamp(0.0, 1.0);
        let channel = |a: u8, b: u8| {
            (f64::from(a) * (1.0 - amount) + f64::from(b) * amount)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Rgb(
            channel(self.0, other.0),
            channel(self.1, other.1),
            channel(self.2, other.2),
        )
    }

    /// WCAG relative luminance, 0 (black) to 1 (white).
    pub fn luminance(self) -> f64 {
        let linear = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.0) + 0.7152 * linear(self.1) + 0.0722 * linear(self.2)
    }

    pub fn to_rgba(self) -> gdk::RGBA {
        gdk::RGBA::new(
            f32::from(self.0) / 255.0,
            f32::from(self.1) / 255.0,
            f32::from(self.2) / 255.0,
            1.0,
        )
    }
}

// ---------------------------------------------------------------------------
// Tokens and themes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeTokens {
    pub name: String,
    pub background: Rgb,
    pub surface: Rgb,
    pub text: Rgb,
    pub accent: Rgb,
    pub danger: Rgb,
    pub muted: Rgb,
    pub highlight: Rgb,
    pub border: Rgb,
    pub dim_text: Rgb,
}

impl ThemeTokens {
    pub fn is_dark(&self) -> bool {
        self.background.luminance() < 0.4
    }

    /// The two squares of the transparency checkerboard.
    pub fn checker(&self) -> (Rgb, Rgb) {
        (
            self.background.mix(self.text, 0.06),
            self.background.mix(self.text, 0.12),
        )
    }
}

/// A theme file as written by a human. Only the three core colours are
/// required; everything else is derived when missing.
#[derive(Debug, Deserialize)]
struct RawTokens {
    name: Option<String>,
    background: String,
    text: String,
    accent: String,
    surface: Option<String>,
    danger: Option<String>,
    muted: Option<String>,
    highlight: Option<String>,
    border: Option<String>,
    dim_text: Option<String>,
}

impl RawTokens {
    fn resolve(self, fallback_name: &str) -> Result<ThemeTokens, String> {
        let required = |key: &str, value: &str| {
            Rgb::parse(value).ok_or_else(|| {
                format!("`{key}` must be a hex colour like #1e1e2e, found {value:?}")
            })
        };
        let optional = |key: &str, value: &Option<String>| -> Result<Option<Rgb>, String> {
            value.as_deref().map(|v| required(key, v)).transpose()
        };

        let background = required("background", &self.background)?;
        let text = required("text", &self.text)?;
        let accent = required("accent", &self.accent)?;
        let dark = background.luminance() < 0.4;

        let surface = optional("surface", &self.surface)?
            .unwrap_or_else(|| background.mix(Rgb::BLACK, if dark { 0.2 } else { 0.04 }));
        let danger = optional("danger", &self.danger)?.unwrap_or(if dark {
            Rgb(0xe0, 0x6c, 0x75)
        } else {
            Rgb(0xd2, 0x0f, 0x39)
        });
        let muted = optional("muted", &self.muted)?.unwrap_or_else(|| background.mix(text, 0.10));
        let highlight = optional("highlight", &self.highlight)?.unwrap_or(muted);
        let border =
            optional("border", &self.border)?.unwrap_or_else(|| background.mix(text, 0.20));
        let dim_text =
            optional("dim_text", &self.dim_text)?.unwrap_or_else(|| background.mix(text, 0.50));

        Ok(ThemeTokens {
            name: self
                .name
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| fallback_name.to_owned()),
            background,
            surface,
            text,
            accent,
            danger,
            muted,
            highlight,
            border,
            dim_text,
        })
    }
}

#[derive(Deserialize)]
struct Catalog {
    themes: Vec<CatalogEntry>,
}

#[derive(Deserialize)]
struct CatalogEntry {
    id: String,
    #[serde(flatten)]
    tokens: RawTokens,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThemeKind {
    System,
    BuiltIn,
    Custom,
}

#[derive(Clone, Debug)]
pub struct ThemeInfo {
    pub id: String,
    pub name: String,
    pub kind: ThemeKind,
}

fn builtin_themes() -> Vec<(String, ThemeTokens)> {
    let catalog: Catalog = serde_yaml_ng::from_str(CATALOG).expect("data/themes.yml is valid");
    catalog
        .themes
        .into_iter()
        .map(|entry| {
            let tokens = entry
                .tokens
                .resolve(&entry.id)
                .unwrap_or_else(|error| panic!("built-in theme {}: {error}", entry.id));
            (entry.id, tokens)
        })
        .collect()
}

/// Ids are file stems; never let one escape the themes directory.
fn is_safe_theme_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0'])
}

const THEME_EXTENSIONS: [&str; 2] = ["yml", "yaml"];

fn custom_theme_path(id: &str) -> PathBuf {
    let dir = settings::themes_dir();
    THEME_EXTENSIONS
        .iter()
        .map(|ext| dir.join(format!("{id}.{ext}")))
        .find(|path| path.exists())
        .unwrap_or_else(|| dir.join(format!("{id}.yml")))
}

fn load_custom_theme(id: &str) -> Option<Result<ThemeTokens, String>> {
    if !is_safe_theme_id(id) {
        return None;
    }
    let path = custom_theme_path(id);
    let text = fs::read_to_string(&path).ok()?;
    Some(
        serde_yaml_ng::from_str::<RawTokens>(&text)
            .map_err(|error| error.to_string())
            .and_then(|raw| raw.resolve(id)),
    )
}

fn custom_theme_ids() -> Vec<String> {
    let Ok(entries) = fs::read_dir(settings::themes_dir()) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !THEME_EXTENSIONS.contains(&path.extension()?.to_str()?) {
                return None;
            }
            Some(path.file_stem()?.to_str()?.to_owned())
        })
        .filter(|id| is_safe_theme_id(id))
        .collect();
    ids.sort();
    ids
}

/// Everything the theme picker offers: System, then built-ins, then the user's own.
pub fn available_themes() -> Vec<ThemeInfo> {
    let mut themes = vec![ThemeInfo {
        id: SYSTEM_THEME_ID.to_owned(),
        name: "System".to_owned(),
        kind: ThemeKind::System,
    }];
    themes.extend(builtin_themes().into_iter().map(|(id, tokens)| ThemeInfo {
        id,
        name: tokens.name,
        kind: ThemeKind::BuiltIn,
    }));
    for id in custom_theme_ids() {
        let name = match load_custom_theme(&id) {
            Some(Ok(tokens)) => tokens.name,
            _ => id.clone(),
        };
        themes.push(ThemeInfo {
            id,
            name,
            kind: ThemeKind::Custom,
        });
    }
    themes
}

fn default_tokens() -> ThemeTokens {
    builtin_themes()
        .into_iter()
        .find(|(id, _)| id == DEFAULT_THEME_ID)
        .map(|(_, tokens)| tokens)
        .expect("the default theme exists in the catalog")
}

/// Resolves a theme id to tokens. Unknown or broken themes fall back to the
/// default, with the reason on stderr.
pub fn resolve(id: &str) -> ThemeTokens {
    if id == SYSTEM_THEME_ID {
        return system_tokens().unwrap_or_else(|| {
            eprintln!("previewer: no colours found in ~/.config/gtk-4.0, using {DEFAULT_THEME_ID}");
            default_tokens()
        });
    }
    match load_custom_theme(id) {
        Some(Ok(tokens)) => return tokens,
        Some(Err(error)) => {
            eprintln!("previewer: theme {id}: {error}");
            return default_tokens();
        }
        None => {}
    }
    if let Some((_, tokens)) = builtin_themes().into_iter().find(|(known, _)| known == id) {
        return tokens;
    }
    eprintln!("previewer: unknown theme {id:?}, using {DEFAULT_THEME_ID}");
    default_tokens()
}

// ---------------------------------------------------------------------------
// System theme: colours published through GTK's `@define-color`
// ---------------------------------------------------------------------------

fn system_tokens() -> Option<ThemeTokens> {
    let gtk_css = glib::user_config_dir().join("gtk-4.0").join("gtk.css");
    let mut colors = HashMap::new();
    collect_defines(&gtk_css, 0, &|p| fs::read_to_string(p).ok(), &mut colors);
    tokens_from_gtk_colors(&colors)
}

/// Strips `/* ... */` comments.
fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn import_target(statement: &str) -> Option<&str> {
    let rest = statement.strip_prefix("@import")?.trim();
    let rest = rest
        .strip_prefix("url(")
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or(rest)
        .trim();
    let rest = rest.trim_matches(['"', '\'']);
    (!rest.is_empty()).then_some(rest)
}

/// Follows `@import`s in file order, recording `@define-color name value;`.
fn collect_defines(
    path: &Path,
    depth: usize,
    read: &dyn Fn(&Path) -> Option<String>,
    colors: &mut HashMap<String, String>,
) {
    if depth > MAX_IMPORT_DEPTH {
        return;
    }
    let Some(text) = read(path) else {
        return;
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    for statement in strip_comments(&text).split(';') {
        let statement = statement.trim();
        if let Some(target) = import_target(statement) {
            let target = Path::new(target);
            let resolved = if target.is_absolute() {
                target.to_path_buf()
            } else {
                dir.join(target)
            };
            collect_defines(&resolved, depth + 1, read, colors);
        } else if let Some(rest) = statement.strip_prefix("@define-color") {
            let mut parts = rest.trim().splitn(2, char::is_whitespace);
            if let (Some(name), Some(value)) = (parts.next(), parts.next()) {
                colors.insert(name.to_owned(), value.trim().to_owned());
            }
        }
    }
}

fn lookup_color(colors: &HashMap<String, String>, name: &str, depth: usize) -> Option<Rgb> {
    if depth > MAX_REFERENCE_DEPTH {
        return None;
    }
    let value = colors.get(name)?;
    match value.strip_prefix('@') {
        Some(reference) => lookup_color(colors, reference.trim(), depth + 1),
        None => Rgb::parse(value),
    }
}

fn first_color(colors: &HashMap<String, String>, names: &[&str]) -> Option<Rgb> {
    names.iter().find_map(|name| lookup_color(colors, name, 0))
}

fn tokens_from_gtk_colors(colors: &HashMap<String, String>) -> Option<ThemeTokens> {
    let background = first_color(
        colors,
        &["window_bg_color", "view_bg_color", "theme_bg_color"],
    )?;
    let text = first_color(
        colors,
        &["window_fg_color", "view_fg_color", "theme_fg_color"],
    )?;
    let accent = first_color(
        colors,
        &["accent_bg_color", "accent_color", "theme_selected_bg_color"],
    )?;
    let raw = |color: Option<Rgb>| color.map(Rgb::hex);
    RawTokens {
        name: Some("System".to_owned()),
        background: background.hex(),
        text: text.hex(),
        accent: accent.hex(),
        surface: raw(first_color(
            colors,
            &["sidebar_bg_color", "popover_bg_color", "card_bg_color"],
        )),
        danger: raw(first_color(
            colors,
            &["destructive_bg_color", "error_bg_color", "error_color"],
        )),
        muted: None,
        highlight: None,
        border: None,
        dim_text: None,
    }
    .resolve("System")
    .ok()
}

// ---------------------------------------------------------------------------
// CSS generation
// ---------------------------------------------------------------------------

/// Keeps a user supplied font name from breaking out of its CSS string.
fn css_string_content(value: &str) -> String {
    value
        .chars()
        .filter(|c| !matches!(c, '"' | '\'' | '\\' | ';' | '{' | '}' | '\n' | '\r'))
        .collect()
}

fn radius(base: f64, roundness: u32) -> u32 {
    (base * f64::from(roundness) / 30.0).round() as u32
}

fn tokens_css(tokens: &ThemeTokens, settings: &Settings) -> String {
    let mut css = format!(
        "@define-color pv_bg {};\n\
         @define-color pv_surface {};\n\
         @define-color pv_text {};\n\
         @define-color pv_accent {};\n\
         @define-color pv_danger {};\n\
         @define-color pv_muted {};\n\
         @define-color pv_highlight {};\n\
         @define-color pv_border {};\n\
         @define-color pv_dim_text {};\n",
        tokens.background.hex(),
        tokens.surface.hex(),
        tokens.text.hex(),
        tokens.accent.hex(),
        tokens.danger.hex(),
        tokens.muted.hex(),
        tokens.highlight.hex(),
        tokens.border.hex(),
        tokens.dim_text.hex(),
    );

    css.push_str(&format!(
        "window, popover, tooltip {{ font-size: {}px; }}\n",
        settings.text_size
    ));
    if let Some(font) = settings.interface_font.as_deref() {
        let font = css_string_content(font);
        if !font.is_empty() {
            css.push_str(&format!(
                "window, popover, tooltip {{ font-family: \"{font}\"; }}\n"
            ));
        }
    }

    let small = radius(5.0, settings.corner_roundness);
    let medium = radius(9.0, settings.corner_roundness);
    css.push_str(&format!(
        "button, entry, tooltip, .thumb, .menu-item, listview > row {{ border-radius: {small}px; }}\n\
         popover > contents {{ border-radius: {medium}px; }}\n"
    ));
    css
}

// ---------------------------------------------------------------------------
// Live theme state
// ---------------------------------------------------------------------------

thread_local! {
    static CURRENT: RefCell<Option<ThemeTokens>> = const { RefCell::new(None) };
    static LISTENERS: RefCell<Vec<Rc<dyn Fn() -> glib::ControlFlow>>> = const { RefCell::new(Vec::new()) };
    static MANAGER: RefCell<Option<Rc<ThemeManager>>> = const { RefCell::new(None) };
}

/// Colours of the transparency checkerboard for the active theme.
pub fn checker_colors() -> (gdk::RGBA, gdk::RGBA) {
    CURRENT.with(|current| match current.borrow().as_ref() {
        Some(tokens) => {
            let (a, b) = tokens.checker();
            (a.to_rgba(), b.to_rgba())
        }
        None => (
            gdk::RGBA::new(0.5, 0.5, 0.5, 1.0),
            gdk::RGBA::new(0.6, 0.6, 0.6, 1.0),
        ),
    })
}

/// Runs `listener` after every theme or settings reload, until it returns
/// `ControlFlow::Break` (typically because the widget it serves is gone).
pub fn connect_changed(listener: impl Fn() -> glib::ControlFlow + 'static) {
    LISTENERS.with(|listeners| listeners.borrow_mut().push(Rc::new(listener)));
}

fn notify_changed() {
    let listeners: Vec<_> = LISTENERS.with(|l| l.borrow().clone());
    let finished: Vec<_> = listeners
        .iter()
        .filter(|listener| listener() == glib::ControlFlow::Break)
        .cloned()
        .collect();
    if !finished.is_empty() {
        LISTENERS.with(|l| {
            l.borrow_mut()
                .retain(|listener| !finished.iter().any(|done| Rc::ptr_eq(done, listener)));
        });
    }
}

/// The installed manager, once `ThemeManager::install` has run.
pub fn manager() -> Option<Rc<ThemeManager>> {
    MANAGER.with(|m| m.borrow().clone())
}

pub struct ThemeManager {
    tokens_provider: gtk::CssProvider,
    user_provider: gtk::CssProvider,
    original_animations: bool,
    settings: RefCell<Settings>,
    monitors: RefCell<Vec<gio::FileMonitor>>,
    themes_monitor: RefCell<Option<gio::FileMonitor>>,
    on_change: RefCell<Option<Rc<dyn Fn()>>>,
}

impl ThemeManager {
    pub fn install(display: &gdk::Display) -> Rc<Self> {
        let style_provider = gtk::CssProvider::new();
        log_css_errors(&style_provider, "built-in stylesheet");
        style_provider.load_from_string(STYLE_CSS);

        let tokens_provider = gtk::CssProvider::new();
        log_css_errors(&tokens_provider, "generated theme");

        let user_provider = gtk::CssProvider::new();
        log_css_errors(&user_provider, "user style.css");

        gtk::style_context_add_provider_for_display(display, &style_provider, STYLE_PRIORITY);
        gtk::style_context_add_provider_for_display(display, &tokens_provider, TOKEN_PRIORITY);
        gtk::style_context_add_provider_for_display(display, &user_provider, USER_STYLE_PRIORITY);

        let original_animations = gtk::Settings::for_display(display).is_gtk_enable_animations();

        let manager = Rc::new(Self {
            tokens_provider,
            user_provider,
            original_animations,
            settings: RefCell::new(Settings::default()),
            monitors: RefCell::new(Vec::new()),
            themes_monitor: RefCell::new(None),
            on_change: RefCell::new(None),
        });
        MANAGER.with(|m| *m.borrow_mut() = Some(manager.clone()));
        manager.reload();
        manager.watch();
        manager
    }

    /// The settings as of the last reload.
    pub fn settings(&self) -> Settings {
        self.settings.borrow().clone()
    }

    /// Re-reads settings, theme files and the user stylesheet, and restyles
    /// every window.
    pub fn reload(&self) {
        let settings = Settings::load();
        let tokens = resolve(&settings.theme);

        self.tokens_provider
            .load_from_string(&tokens_css(&tokens, &settings));

        let user_css = settings::user_style_path();
        match fs::read_to_string(&user_css) {
            Ok(text) => self.user_provider.load_from_string(&text),
            Err(_) => self.user_provider.load_from_string(""),
        }

        if let Some(gtk_settings) = gtk::Settings::default() {
            set_color_scheme(&gtk_settings, tokens.is_dark());
            gtk_settings
                .set_gtk_enable_animations(self.original_animations && !settings.reduce_motion);
        }

        CURRENT.with(|current| *current.borrow_mut() = Some(tokens));
        *self.settings.borrow_mut() = settings;
        self.watch_themes_dir();
        notify_changed();
    }

    /// Chooses a theme, remembers the choice and applies it immediately.
    pub fn select_theme(&self, id: &str) {
        if let Err(error) = settings::save_theme(id) {
            eprintln!("previewer: cannot save theme choice: {error}");
        }
        self.reload();
    }

    fn watch(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let on_change = debounced(move || {
            if let Some(manager) = weak.upgrade() {
                manager.reload();
            }
        });

        // Settings and style.css live here; the GTK directory carries the
        // colours the "system" theme follows.
        for dir in [
            settings::config_dir(),
            glib::user_config_dir().join("gtk-4.0"),
        ] {
            if let Some(monitor) = monitor_dir(&dir, on_change.clone()) {
                self.monitors.borrow_mut().push(monitor);
            }
        }
        *self.on_change.borrow_mut() = Some(on_change);
        self.watch_themes_dir();
    }

    /// Watches `themes/` once it exists, so dropping in a theme file shows up
    /// in the picker and editing the active one restyles live.
    fn watch_themes_dir(&self) {
        if self.themes_monitor.borrow().is_some() {
            return;
        }
        let Some(on_change) = self.on_change.borrow().clone() else {
            return;
        };
        *self.themes_monitor.borrow_mut() = monitor_dir(&settings::themes_dir(), on_change);
    }
}

/// Tells GTK whether the theme is dark, so widgets the stylesheet does not
/// cover (scrollbar fallbacks, file dialogs) pick a matching base. GTK 4.20
/// replaced the old boolean with `gtk-interface-color-scheme`; look it up at
/// runtime so older GTK keeps working.
fn set_color_scheme(settings: &gtk::Settings, dark: bool) {
    let nick = if dark { "dark" } else { "light" };
    let value = settings
        .find_property("gtk-interface-color-scheme")
        .and_then(|pspec| glib::EnumClass::with_type(pspec.value_type()))
        .and_then(|class| class.to_value_by_nick(nick));
    match value {
        Some(value) => settings.set_property_from_value("gtk-interface-color-scheme", &value),
        None => {
            #[allow(deprecated)]
            settings.set_gtk_application_prefer_dark_theme(dark);
        }
    }
}

fn monitor_dir(dir: &Path, on_change: Rc<dyn Fn()>) -> Option<gio::FileMonitor> {
    if !dir.is_dir() {
        return None;
    }
    let file = gio::File::for_path(dir);
    match file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE) {
        Ok(monitor) => {
            monitor.connect_changed(move |_, _, _, _| on_change());
            Some(monitor)
        }
        Err(error) => {
            eprintln!("previewer: cannot watch {}: {error}", dir.display());
            None
        }
    }
}

/// Collapses a burst of file events (editors write in several steps) into one call.
fn debounced(action: impl Fn() + 'static) -> Rc<dyn Fn()> {
    let action = Rc::new(action);
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    Rc::new(move || {
        if let Some(id) = pending.borrow_mut().take() {
            id.remove();
        }
        let action = action.clone();
        let slot = pending.clone();
        let id = glib::timeout_add_local_once(RELOAD_DEBOUNCE, move || {
            slot.borrow_mut().take();
            action();
        });
        *pending.borrow_mut() = Some(id);
    })
}

fn log_css_errors(provider: &gtk::CssProvider, what: &'static str) {
    provider.connect_parsing_error(move |_, section, error| {
        eprintln!("previewer: CSS error in {what} at {section}: {error}");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gtk_colors(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn parses_hex_colours() {
        assert_eq!(Rgb::parse("#1e1e2e"), Some(Rgb(0x1e, 0x1e, 0x2e)));
        assert_eq!(Rgb::parse("  #FFF "), Some(Rgb(255, 255, 255)));
        assert_eq!(Rgb::parse("#11223344"), Some(Rgb(0x11, 0x22, 0x33)));
        assert_eq!(Rgb::parse("1e1e2e"), None);
        assert_eq!(Rgb::parse("#12345"), None);
        assert_eq!(Rgb::parse("#gggggg"), None);
        assert_eq!(Rgb::parse("#é1é1é1"), None);
        assert_eq!(Rgb::parse("red"), None);
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(Rgb(0x1e, 0x1e, 0x2e).hex(), "#1e1e2e");
        assert_eq!(Rgb::parse(&Rgb(1, 2, 3).hex()), Some(Rgb(1, 2, 3)));
    }

    #[test]
    fn mixing_blends_linearly() {
        assert_eq!(Rgb(0, 0, 0).mix(Rgb(255, 255, 255), 0.0), Rgb(0, 0, 0));
        assert_eq!(
            Rgb(0, 0, 0).mix(Rgb(255, 255, 255), 1.0),
            Rgb(255, 255, 255)
        );
        assert_eq!(Rgb(0, 0, 0).mix(Rgb(200, 100, 50), 0.5), Rgb(100, 50, 25));
        assert_eq!(
            Rgb(0, 0, 0).mix(Rgb(255, 255, 255), 9.0),
            Rgb(255, 255, 255)
        );
    }

    #[test]
    fn luminance_orders_light_and_dark() {
        assert!(Rgb(0, 0, 0).luminance() < 0.01);
        assert!(Rgb(255, 255, 255).luminance() > 0.99);
    }

    #[test]
    fn every_builtin_theme_resolves() {
        let themes = builtin_themes();
        assert!(themes.len() >= 9);
        assert!(themes.iter().any(|(id, _)| id == DEFAULT_THEME_ID));
        let mut ids: Vec<_> = themes.iter().map(|(id, _)| id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), themes.len(), "theme ids are unique");
    }

    #[test]
    fn builtin_dark_and_light_are_classified() {
        let themes: HashMap<_, _> = builtin_themes().into_iter().collect();
        assert!(themes["catppuccin-mocha"].is_dark());
        assert!(!themes["catppuccin-latte"].is_dark());
    }

    #[test]
    fn minimal_theme_derives_the_rest() {
        let raw: RawTokens = serde_yaml_ng::from_str(
            "background: \"#101010\"\ntext: \"#f0f0f0\"\naccent: \"#ff8800\"",
        )
        .unwrap();
        let tokens = raw.resolve("mine").unwrap();
        assert_eq!(tokens.name, "mine");
        assert!(tokens.is_dark());
        assert!(tokens.surface.luminance() < tokens.background.luminance());
        assert!(tokens.dim_text.luminance() > tokens.background.luminance());
        assert!(tokens.dim_text.luminance() < tokens.text.luminance());
    }

    #[test]
    fn bad_colours_are_rejected_not_interpolated() {
        let raw: RawTokens = serde_yaml_ng::from_str(
            "background: \"#101010\"\ntext: \"red; } * { display: none\"\naccent: \"#ff8800\"",
        )
        .unwrap();
        let error = raw.resolve("x").unwrap_err();
        assert!(error.contains("text"), "{error}");
    }

    #[test]
    fn missing_required_colour_is_a_parse_error() {
        assert!(serde_yaml_ng::from_str::<RawTokens>("background: \"#000000\"").is_err());
    }

    #[test]
    fn theme_ids_cannot_escape_the_themes_dir() {
        assert!(is_safe_theme_id("nord"));
        assert!(!is_safe_theme_id("../secrets"));
        assert!(!is_safe_theme_id("a/b"));
        assert!(!is_safe_theme_id(".."));
        assert!(!is_safe_theme_id(""));
    }

    #[test]
    fn gtk_colours_map_to_tokens() {
        let colors = gtk_colors(&[
            ("window_bg_color", "#24273a"),
            ("window_fg_color", "#cad3f5"),
            ("accent_bg_color", "#8aadf4"),
            ("sidebar_bg_color", "#363a4f"),
            ("destructive_bg_color", "#ed8796"),
        ]);
        let tokens = tokens_from_gtk_colors(&colors).unwrap();
        assert_eq!(tokens.background, Rgb(0x24, 0x27, 0x3a));
        assert_eq!(tokens.text, Rgb(0xca, 0xd3, 0xf5));
        assert_eq!(tokens.accent, Rgb(0x8a, 0xad, 0xf4));
        assert_eq!(tokens.surface, Rgb(0x36, 0x3a, 0x4f));
        assert_eq!(tokens.danger, Rgb(0xed, 0x87, 0x96));
    }

    #[test]
    fn gtk_colours_follow_references() {
        let colors = gtk_colors(&[
            ("window_bg_color", "#000000"),
            ("window_fg_color", "#ffffff"),
            ("accent_bg_color", "@brand"),
            ("brand", "@brand2"),
            ("brand2", "#123456"),
        ]);
        assert_eq!(
            tokens_from_gtk_colors(&colors).unwrap().accent,
            Rgb(0x12, 0x34, 0x56)
        );
    }

    #[test]
    fn reference_cycles_terminate() {
        let colors = gtk_colors(&[("a", "@b"), ("b", "@a")]);
        assert_eq!(lookup_color(&colors, "a", 0), None);
    }

    #[test]
    fn missing_core_colours_means_no_system_theme() {
        assert!(tokens_from_gtk_colors(&gtk_colors(&[("window_bg_color", "#000000")])).is_none());
    }

    #[test]
    fn imports_and_comments_are_followed() {
        let files: HashMap<PathBuf, String> = HashMap::from([
            (
                PathBuf::from("/cfg/gtk.css"),
                "/* @define-color window_bg_color #ff0000; */\n@import url(\"noctalia.css\");\n@define-color accent_bg_color #00ff00;".to_owned(),
            ),
            (
                PathBuf::from("/cfg/noctalia.css"),
                "@define-color window_bg_color #24273a;\n@define-color window_fg_color #cad3f5;\n@import 'sub/more.css';".to_owned(),
            ),
            (
                PathBuf::from("/cfg/sub/more.css"),
                "@define-color accent_bg_color #8aadf4;".to_owned(),
            ),
        ]);
        let mut colors = HashMap::new();
        collect_defines(
            Path::new("/cfg/gtk.css"),
            0,
            &|p| files.get(p).cloned(),
            &mut colors,
        );
        assert_eq!(colors["window_bg_color"], "#24273a");
        assert_eq!(colors["window_fg_color"], "#cad3f5");
        // gtk.css defines accent after the import, so it wins.
        assert_eq!(colors["accent_bg_color"], "#00ff00");
    }

    #[test]
    fn import_cycles_terminate() {
        let files: HashMap<PathBuf, String> = HashMap::from([
            (PathBuf::from("/a.css"), "@import \"b.css\";".to_owned()),
            (PathBuf::from("/b.css"), "@import \"a.css\";".to_owned()),
        ]);
        let mut colors = HashMap::new();
        collect_defines(
            Path::new("/a.css"),
            0,
            &|p| files.get(p).cloned(),
            &mut colors,
        );
        assert!(colors.is_empty());
    }

    #[test]
    fn css_font_names_cannot_inject() {
        assert_eq!(css_string_content("Fira \"Code\"; } x {"), "Fira Code  x ");
    }

    #[test]
    fn roundness_scales_radii() {
        assert_eq!(radius(5.0, 30), 5);
        assert_eq!(radius(5.0, 0), 0);
        assert_eq!(radius(9.0, 30), 9);
        assert_eq!(radius(5.0, 60), 10);
    }

    #[test]
    fn generated_css_defines_every_token() {
        let tokens = default_tokens();
        let css = tokens_css(&tokens, &Settings::default());
        for name in [
            "pv_bg",
            "pv_surface",
            "pv_text",
            "pv_accent",
            "pv_danger",
            "pv_muted",
            "pv_highlight",
            "pv_border",
            "pv_dim_text",
        ] {
            assert!(css.contains(&format!("@define-color {name} #")), "{name}");
        }
        assert!(css.contains("font-size: 13px"));
    }
}
