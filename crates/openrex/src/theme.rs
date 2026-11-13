//! Colors: terminal themes sent to the server with set_theme, the window's
//! colours derived from them, client.theme.change, and the settings file
//! that remembers the theme and font between runs.
//!
//! A theme lives on the server, per terminal: OpenRex sends it to every
//! terminal it shows with `com.superlogical.terminal.set_theme`, and the
//! server sends it back down each terminal's stream, where the Mirror picks
//! it up. So the pane's painting follows without this module touching it.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

use gpui::{Context, Hsla, Window};
use serde_json::{Map, Value, json};

use crate::actions::{Outcome, Target};
use crate::palette::{Item, Palette};
use crate::pane::hsla;
use crate::term::{Rgb, parse_hex};
use crate::workspace::Workspace;

/// The theme the window starts with when none was chosen: the colours
/// OpenRex has always drawn its chrome in.
pub const DEFAULT_THEME: &str = "Tokyo Night";

const SET_THEME: &str = "com.superlogical.terminal.set_theme";
const TERMINAL: &str = "com.superlogical.terminal";

/// Where a theme came from, which decides which one wins when two share a
/// name: your own files, then the built-in ones, then Ghostty's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Builtin,
    /// Ghostty's themes, installed with it.
    System,
    /// Your own Ghostty themes, in ~/.config/ghostty/themes.
    User,
}

/// A terminal colour theme.
#[derive(Clone, Debug, PartialEq)]
pub struct TermTheme {
    pub name: String,
    pub source: Source,
    pub background: Option<Rgb>,
    pub foreground: Option<Rgb>,
    pub cursor: Option<Rgb>,
    pub selection: Option<Rgb>,
    /// Palette entries the theme sets, by index. The rest keep the
    /// terminal's built-in colours.
    pub palette: Vec<(u8, Rgb)>,
}

impl TermTheme {
    /// A palette colour the theme sets.
    pub fn color(&self, index: u8) -> Option<Rgb> {
        self.palette.iter().rev().find(|(i, _)| *i == index).map(|(_, c)| *c)
    }

    /// Whether the background is light, so programs and the window chrome
    /// should use dark text. No background means the terminal's own, dark.
    pub fn is_light(&self) -> bool {
        self.background.is_some_and(|bg| luminance(bg) > 0.4)
    }

    /// The arguments of `com.superlogical.terminal.set_theme`. Colours the
    /// theme leaves out go back to the terminal's built-in ones.
    pub fn set_theme_args(&self) -> Value {
        let mut args = json!({
            "scheme": if self.is_light() { "light" } else { "dark" },
            "palette": self.palette.iter().map(|(i, c)| format!("{i}={}", hex(*c))).collect::<Vec<_>>(),
        });
        for (key, color) in [("background", self.background), ("foreground", self.foreground), ("cursor", self.cursor)] {
            args[key] = json!(color.map(hex).unwrap_or_default());
        }
        args
    }
}

/// `#rrggbb`.
pub fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// Reads a theme in Ghostty's format: `key = value` lines, of which the
/// colour ones count (`palette = N=#rrggbb`, `background`, `foreground`,
/// `cursor-color`, `selection-background`). Anything else, and colours that
/// don't read, are skipped. `None` when the file sets no colour at all, as it
/// isn't a theme.
pub fn parse_ghostty(name: &str, source: Source, text: &str) -> Option<TermTheme> {
    let mut theme = TermTheme {
        name: name.to_string(),
        source,
        background: None,
        foreground: None,
        cursor: None,
        selection: None,
        palette: Vec::new(),
    };
    let mut any = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim().trim_matches('"').trim();
        let slot = match key.trim() {
            "background" => &mut theme.background,
            "foreground" => &mut theme.foreground,
            "cursor-color" => &mut theme.cursor,
            "selection-background" => &mut theme.selection,
            "palette" => {
                let Some((index, color)) = value.split_once('=') else { continue };
                if let (Ok(index), Some(color)) = (index.trim().parse::<u8>(), parse_hex(color)) {
                    theme.palette.retain(|(i, _)| *i != index);
                    theme.palette.push((index, color));
                    any = true;
                }
                continue;
            }
            _ => continue,
        };
        if let Some(color) = parse_hex(value) {
            *slot = Some(color);
            any = true;
        }
    }
    theme.palette.sort_by_key(|(i, _)| *i);
    any.then_some(theme)
}

/// A built-in theme's colours, as published by its authors.
struct Builtin {
    name: &'static str,
    background: u32,
    foreground: u32,
    cursor: u32,
    selection: u32,
    palette: [u32; 16],
}

const BUILTINS: &[Builtin] = &[
    // folke/tokyonight.nvim, the "night" style.
    Builtin {
        name: "Tokyo Night",
        background: 0x1a1b26,
        foreground: 0xc0caf5,
        cursor: 0xc0caf5,
        selection: 0x283457,
        palette: [
            0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
            0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
        ],
    },
    Builtin {
        name: "Catppuccin Mocha",
        background: 0x1e1e2e,
        foreground: 0xcdd6f4,
        cursor: 0xf5e0dc,
        selection: 0x585b70,
        palette: [
            0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
            0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
        ],
    },
    Builtin {
        name: "Catppuccin Latte",
        background: 0xeff1f5,
        foreground: 0x4c4f69,
        cursor: 0xdc8a78,
        selection: 0xacb0be,
        palette: [
            0x5c5f77, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xacb0be,
            0x6c6f85, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xbcc0cc,
        ],
    },
    Builtin {
        name: "Dracula",
        background: 0x282a36,
        foreground: 0xf8f8f2,
        cursor: 0xf8f8f2,
        selection: 0x44475a,
        palette: [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ],
    },
    // morhetz/gruvbox, dark with medium contrast.
    Builtin {
        name: "Gruvbox Dark",
        background: 0x282828,
        foreground: 0xebdbb2,
        cursor: 0xebdbb2,
        selection: 0x504945,
        palette: [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ],
    },
    Builtin {
        name: "Nord",
        background: 0x2e3440,
        foreground: 0xd8dee9,
        cursor: 0xd8dee9,
        selection: 0x434c5e,
        palette: [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
            0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
        ],
    },
    // Atom's One Dark, as ported to terminals.
    Builtin {
        name: "One Dark",
        background: 0x282c34,
        foreground: 0xabb2bf,
        cursor: 0x528bff,
        selection: 0x3e4451,
        palette: [
            0x1e2127, 0xe06c75, 0x98c379, 0xd19a66, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
            0x5c6370, 0xe06c75, 0x98c379, 0xd19a66, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
        ],
    },
    // Ethan Schoonover's terminal mapping of the Solarized palette.
    Builtin {
        name: "Solarized Dark",
        background: 0x002b36,
        foreground: 0x839496,
        cursor: 0x839496,
        selection: 0x073642,
        palette: [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
            0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
        ],
    },
    Builtin {
        name: "Solarized Light",
        background: 0xfdf6e3,
        foreground: 0x657b83,
        cursor: 0x657b83,
        selection: 0xeee8d5,
        palette: [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
            0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
        ],
    },
    Builtin {
        name: "Rosé Pine",
        background: 0x191724,
        foreground: 0xe0def4,
        cursor: 0x524f67,
        selection: 0x403d52,
        palette: [
            0x26233a, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
            0x6e6a86, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
        ],
    },
];

fn rgb_of(n: u32) -> Rgb {
    Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
}

impl Builtin {
    fn theme(&self) -> TermTheme {
        TermTheme {
            name: self.name.to_string(),
            source: Source::Builtin,
            background: Some(rgb_of(self.background)),
            foreground: Some(rgb_of(self.foreground)),
            cursor: Some(rgb_of(self.cursor)),
            selection: Some(rgb_of(self.selection)),
            palette: self.palette.iter().enumerate().map(|(i, c)| (i as u8, rgb_of(*c))).collect(),
        }
    }
}

pub fn builtin_themes() -> Vec<TermTheme> {
    BUILTINS.iter().map(Builtin::theme).collect()
}

/// The folders Ghostty themes are read from, Ghostty's own then yours.
pub fn theme_dirs() -> Vec<(PathBuf, Source)> {
    let mut dirs = vec![(PathBuf::from("/usr/share/ghostty/themes"), Source::System)];
    if let Some(config) = config_home() {
        dirs.push((config.join("ghostty/themes"), Source::User));
    }
    dirs
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

/// Every theme file in a folder. A file's name is its theme's name, as in
/// Ghostty. Unreadable files, and ones too big to be a theme, are skipped.
fn read_theme_dir(dir: &Path, source: Source) -> Vec<TermTheme> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let meta = std::fs::metadata(entry.path()).ok()?;
            if name.starts_with('.') || !meta.is_file() || meta.len() > 64 * 1024 {
                return None;
            }
            let text = std::fs::read_to_string(entry.path()).ok()?;
            parse_ghostty(&name, source, &text)
        })
        .collect()
}

/// Every theme: the built-in ones first, then the files of `dirs` by name.
/// Of two themes with one name, your own file wins over a built-in theme,
/// which wins over Ghostty's.
pub fn catalog_from(dirs: &[(PathBuf, Source)]) -> Vec<TermTheme> {
    let files: Vec<TermTheme> = dirs.iter().flat_map(|(dir, source)| read_theme_dir(dir, *source)).collect();
    merge(builtin_themes(), files)
}

fn merge(mut themes: Vec<TermTheme>, files: Vec<TermTheme>) -> Vec<TermTheme> {
    let key = |t: &TermTheme| t.name.to_lowercase();
    let builtin_count = themes.len();
    // Your own files replace a built-in theme of the same name in place.
    for file in files.iter().filter(|t| t.source == Source::User) {
        if let Some(slot) = themes[..builtin_count].iter_mut().find(|t| key(&**t) == key(file)) {
            *slot = file.clone();
        }
    }
    let mut rest: Vec<TermTheme> = Vec::new();
    let rank = |s: Source| if s == Source::User { 0 } else { 1 };
    let mut files = files;
    files.sort_by_key(|t| rank(t.source));
    for file in files {
        let k = key(&file);
        if themes[..builtin_count].iter().any(|t| key(t) == k) || rest.iter().any(|t| key(t) == k) {
            continue;
        }
        rest.push(file);
    }
    rest.sort_by_cached_key(key);
    themes.extend(rest);
    themes
}

pub fn catalog() -> Vec<TermTheme> {
    catalog_from(&theme_dirs())
}

/// A name with only its letters and digits, lowercased, so that `rose-pine`
/// finds Rosé Pine.
fn compact(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'é' | 'É' | 'è' | 'ê' => 'e',
            c => c,
        })
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The theme a name means: the one with that name, ignoring case, or else
/// the first whose name matches ignoring spaces, punctuation, and accents.
pub fn find_in<'a>(themes: &'a [TermTheme], name: &str) -> Option<&'a TermTheme> {
    let lower = name.trim().to_lowercase();
    themes.iter().find(|t| t.name.to_lowercase() == lower).or_else(|| {
        let want = compact(name);
        (!want.is_empty()).then(|| themes.iter().find(|t| compact(&t.name) == want)).flatten()
    })
}

pub fn find(name: &str) -> Option<TermTheme> {
    find_in(&catalog(), name).cloned()
}

// The window's colours.

/// The colours of everything around the terminals.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Colors {
    /// Behind the panes.
    pub bg: Rgb,
    /// The sidebar, tab bar, status bar, and palette.
    pub panel: Rgb,
    pub border: Rgb,
    pub text: Rgb,
    /// Text a step quieter than `text`, such as unselected palette rows.
    pub soft: Rgb,
    pub dim: Rgb,
    pub accent: Rgb,
    pub danger: Rgb,
    pub ok: Rgb,
    pub warn: Rgb,
    /// Under the pointer.
    pub hover: Rgb,
    /// The chosen row of a list.
    pub selected: Rgb,
}

/// The colours the window has always had, which Tokyo Night keeps exactly.
const TOKYO_NIGHT_CHROME: Colors = Colors {
    bg: Rgb(0x16, 0x16, 0x1e),
    panel: Rgb(0x1a, 0x1b, 0x26),
    border: Rgb(0x2a, 0x2c, 0x3d),
    text: Rgb(0xc0, 0xca, 0xf5),
    soft: Rgb(0xa9, 0xb1, 0xd6),
    dim: Rgb(0x6b, 0x73, 0x94),
    accent: Rgb(0x7a, 0xa2, 0xf7),
    danger: Rgb(0xf7, 0x76, 0x8e),
    ok: Rgb(0x9e, 0xce, 0x6a),
    warn: Rgb(0xe0, 0xaf, 0x68),
    hover: Rgb(0x24, 0x26, 0x3a),
    selected: Rgb(0x2e, 0x32, 0x50),
};

/// `a` moved toward `b` by `t`, from 0 (all `a`) to 1 (all `b`).
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// Relative luminance, as WCAG defines it.
pub fn luminance(c: Rgb) -> f32 {
    let lin = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c.0) + 0.7152 * lin(c.1) + 0.0722 * lin(c.2)
}

/// WCAG contrast ratio, from 1 (none) to 21.
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Window colours that go with a terminal theme: its background for the
/// panels, its text colour for text, and its blue, red, green, and yellow
/// for accents and status. A light theme so gets a light window.
pub fn derive_colors(theme: &TermTheme) -> Colors {
    if theme.source == Source::Builtin && theme.name == DEFAULT_THEME {
        return TOKYO_NIGHT_CHROME;
    }
    let light = theme.is_light();
    let panel = theme.background.unwrap_or(TOKYO_NIGHT_CHROME.panel);
    let text = theme
        .foreground
        .filter(|fg| contrast(*fg, panel) >= 3.0)
        .unwrap_or(if light { Rgb(0x20, 0x20, 0x20) } else { Rgb(0xe8, 0xe8, 0xe8) });
    // The first of the palette colours that reads against the panels.
    let pick = |indices: &[u8], min: f32| {
        indices.iter().filter_map(|i| theme.color(*i)).find(|c| contrast(*c, panel) >= min)
    };
    let accent = pick(&[4, 12, 6, 14, 5], 2.0).unwrap_or(text);
    let mut dim = mix(panel, text, 0.5);
    for step in [0.6, 0.7, 0.8] {
        if contrast(dim, panel) >= 2.5 {
            break;
        }
        dim = mix(panel, text, step);
    }
    Colors {
        bg: mix(panel, Rgb(0, 0, 0), if light { 0.06 } else { 0.16 }),
        panel,
        border: mix(panel, text, if light { 0.16 } else { 0.12 }),
        text,
        soft: mix(panel, text, 0.85),
        dim,
        accent,
        danger: pick(&[1, 9], 2.0).unwrap_or(Rgb(0xd0, 0x30, 0x40)),
        ok: pick(&[2, 10], 1.5).unwrap_or(accent),
        warn: pick(&[3, 11], 1.5).unwrap_or(accent),
        hover: mix(panel, text, 0.08),
        selected: theme.selection.filter(|s| contrast(*s, text) >= 3.0).unwrap_or_else(|| mix(panel, accent, 0.25)),
    }
}

/// The window's colours, as gpui draws them.
#[derive(Clone, Copy, Debug)]
pub struct Chrome {
    pub bg: Hsla,
    pub panel: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub soft: Hsla,
    pub dim: Hsla,
    pub accent: Hsla,
    pub danger: Hsla,
    pub ok: Hsla,
    pub warn: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
}

impl From<Colors> for Chrome {
    fn from(c: Colors) -> Chrome {
        Chrome {
            bg: hsla(c.bg),
            panel: hsla(c.panel),
            border: hsla(c.border),
            text: hsla(c.text),
            soft: hsla(c.soft),
            dim: hsla(c.dim),
            accent: hsla(c.accent),
            danger: hsla(c.danger),
            ok: hsla(c.ok),
            warn: hsla(c.warn),
            hover: hsla(c.hover),
            selected: hsla(c.selected),
        }
    }
}

// What the running client has chosen.

/// The theme in use. One window per process, and the window's drawing code
/// asks for colours from places with no context to hand, so it lives here.
struct State {
    theme: TermTheme,
    chrome: Chrome,
    /// Whether terminals get the theme. Until a theme is chosen they keep
    /// the server's colours, as before themes existed.
    send: bool,
    settings: Option<SettingsFile>,
    /// The terminals sent the theme since it last changed, and the
    /// connection that sent it: a new connection sends it again.
    sent: HashSet<String>,
    client_id: Option<String>,
    /// Counts theme changes, so calls still sending an older theme stop
    /// rather than overwrite a newer one.
    generation: u64,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    let theme = BUILTINS[0].theme();
    Mutex::new(State {
        chrome: derive_colors(&theme).into(),
        theme,
        send: false,
        settings: None,
        sent: HashSet::new(),
        client_id: None,
        generation: 0,
    })
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sets up the theme at startup: the one chosen on the command line or in
/// the settings file, if any, and where to save changes.
pub fn init(settings: Option<SettingsFile>, theme: Option<TermTheme>) {
    let mut s = state();
    s.settings = settings;
    if let Some(theme) = theme {
        s.chrome = derive_colors(&theme).into();
        s.theme = theme;
        s.send = true;
    }
}

/// The window's colours.
pub fn chrome() -> Chrome {
    state().chrome
}

/// The name of the theme in use.
pub fn current_name() -> String {
    state().theme.name.clone()
}

/// Remembers a setting in the settings file. A failure is logged: it
/// shouldn't get in the way of what changed it.
pub fn remember(key: &str, value: Value) {
    let file = state().settings.clone();
    if let Some(file) = file
        && let Err(e) = file.set(key, value)
    {
        log::warn!("saving {key} in {}: {e}", file.path.display());
    }
}

/// Sends the theme to every terminal of the attached sessions that hasn't
/// had it yet. Runs on each render, so new panes get it as they appear.
pub(crate) fn sync(ws: &mut Workspace, cx: &mut Context<Workspace>) {
    let store = ws.store.read(cx);
    let Some(control) = store.control() else { return };
    let mut s = state();
    if !s.send {
        return;
    }
    if s.client_id != store.client_id {
        s.client_id = store.client_id.clone();
        s.sent.clear();
    }
    let terminals: Vec<(String, String)> = store
        .views
        .iter()
        .flat_map(|(session, view)| {
            view.windows
                .iter()
                .flat_map(|w| &w.layers)
                .flat_map(|l| &l.blocks)
                .filter(|b| b.creator_name == TERMINAL)
                .filter_map(move |b| Some((session.clone(), b.block_id.clone()?)))
        })
        .collect();
    // Forget closed terminals, so the set doesn't grow.
    s.sent.retain(|id| terminals.iter().any(|(_, b)| b == id));
    let fresh: Vec<(String, String)> = terminals.into_iter().filter(|(_, b)| s.sent.insert(b.clone())).collect();
    if fresh.is_empty() {
        return;
    }
    let args = s.theme.set_theme_args();
    let generation = s.generation;
    drop(s);
    cx.spawn(async move |_this, _cx| {
        for (session, block) in fresh {
            if state().generation != generation {
                return;
            }
            if let Err(e) = control.block_call(SET_THEME, &session, &block, args.clone()).await {
                log::warn!("{SET_THEME} on {block}: {}", e.message);
            }
        }
    })
    .detach();
}

/// Makes a theme the one in use: the terminals get it, the window takes its
/// colours, and the settings file remembers it.
fn apply(theme: TermTheme, ws: &mut Workspace, cx: &mut Context<Workspace>) {
    let name = theme.name.clone();
    {
        let mut s = state();
        s.chrome = derive_colors(&theme).into();
        s.theme = theme;
        s.send = true;
        s.sent.clear();
        s.generation += 1;
    }
    sync(ws, cx);
    remember("theme", json!(name));
    cx.notify();
}

/// Performs this module's actions. `None` for an action it doesn't own.
pub(crate) fn perform(
    ws: &mut Workspace,
    action: &str,
    args: &Value,
    _target: &Target,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    if action != "client.theme.change" {
        return None;
    }
    let themes = catalog();
    let name = args.get("theme").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
    Some(match name {
        Some(name) => match find_in(&themes, name) {
            Some(theme) => {
                let name = theme.name.clone();
                apply(theme.clone(), ws, cx);
                Ok(json!({"theme": name}))
            }
            None => Err(("not_found".into(), format!("there is no theme named {name}"))),
        },
        None => {
            ws.palette = Some(picker(&themes, &current_name()));
            Ok(Value::Null)
        }
    })
}

/// A palette of every theme, starting on the current one.
fn picker(themes: &[TermTheme], current: &str) -> Palette {
    let items: Vec<Item> = themes
        .iter()
        .map(|t| {
            let from = match t.source {
                Source::Builtin => "built-in",
                Source::System => "Ghostty",
                Source::User => "yours",
            };
            let shade = if t.is_light() { "light" } else { "dark" };
            Item::new(t.name.clone(), "client.theme.change", json!({"theme": t.name}))
                .detail(format!("{shade} · {from}"))
                .hint(if t.name == current { "current" } else { "" })
                .keywords(shade)
        })
        .collect();
    let mut palette = Palette::pick("Change Theme", items).empty("No theme matches");
    palette.selected = themes.iter().position(|t| t.name == current).unwrap_or(0);
    palette
}

// The settings file.

/// What the settings file holds. Each is `None` when it isn't set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Saved {
    pub theme: Option<String>,
    pub font_family: Option<String>,
    pub font_size: Option<f32>,
}

/// OpenRex's settings, as JSON in `~/.config/openrex/settings.json`.
/// Writing changes one key and keeps the rest, including keys this version
/// doesn't know.
#[derive(Clone, Debug)]
pub struct SettingsFile {
    pub path: PathBuf,
}

impl SettingsFile {
    pub fn new(path: impl Into<PathBuf>) -> SettingsFile {
        SettingsFile { path: path.into() }
    }

    /// The usual place, under `$XDG_CONFIG_HOME` or `~/.config`.
    pub fn standard() -> Option<SettingsFile> {
        config_home().map(|c| SettingsFile::new(c.join("openrex/settings.json")))
    }

    /// Reads the settings. No file means none are set; a file that isn't a
    /// JSON object is an error, for the person to fix.
    pub fn load(&self) -> Result<Saved, String> {
        let Some(v) = self.read()? else { return Ok(Saved::default()) };
        let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        Ok(Saved {
            theme: text("theme"),
            font_family: text("font_family"),
            font_size: v.get("font_size").and_then(Value::as_f64).map(|s| s as f32).filter(|s| (6.0..=72.0).contains(s)),
        })
    }

    fn read(&self) -> Result<Option<Map<String, Value>>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("reading {}: {e}", self.path.display())),
        };
        if text.trim().is_empty() {
            return Ok(Some(Map::new()));
        }
        match serde_json::from_str(&text) {
            Ok(Value::Object(map)) => Ok(Some(map)),
            Ok(_) => Err(format!("{} should hold a JSON object", self.path.display())),
            Err(e) => Err(format!("{} isn't valid JSON: {e}", self.path.display())),
        }
    }

    /// Sets one key and writes the file. The new file is written beside the
    /// old and renamed over it, so a crash leaves one or the other whole. A
    /// file that doesn't read is left alone rather than overwritten.
    pub fn set(&self, key: &str, value: Value) -> Result<(), String> {
        let mut map = self.read()?.unwrap_or_default();
        map.insert(key.to_string(), value);
        let mut text = serde_json::to_string_pretty(&Value::Object(map)).map_err(|e| e.to_string())?;
        text.push('\n');
        let dir = self.path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        let file_name = self.path.file_name().and_then(|n| n.to_str()).unwrap_or("settings.json");
        let tmp = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        };
        write().map_err(|e| {
            std::fs::remove_file(&tmp).ok();
            format!("writing {}: {e}", self.path.display())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOCHA: &str = "\
# A comment, and a key we don't use.
palette = 0=#45475a
palette = 1=#f38ba8
palette = 15 = a6adc8
palette = 300=#ffffff
palette = 2=not-a-colour
background = #1e1e2e
foreground = \"#cdd6f4\"
cursor-color = #f5e0dc
cursor-text = #1e1e2e
selection-background = #585b70
font-family = Iosevka
";

    #[test]
    fn reads_ghostty_themes() {
        let t = parse_ghostty("Mocha", Source::System, MOCHA).unwrap();
        assert_eq!(t.name, "Mocha");
        assert_eq!(t.background, Some(Rgb(0x1e, 0x1e, 0x2e)));
        assert_eq!(t.foreground, Some(Rgb(0xcd, 0xd6, 0xf4)));
        assert_eq!(t.cursor, Some(Rgb(0xf5, 0xe0, 0xdc)));
        assert_eq!(t.selection, Some(Rgb(0x58, 0x5b, 0x70)));
        // Index 300 and the unreadable colour are skipped.
        assert_eq!(
            t.palette,
            vec![(0, Rgb(0x45, 0x47, 0x5a)), (1, Rgb(0xf3, 0x8b, 0xa8)), (15, Rgb(0xa6, 0xad, 0xc8))]
        );
        assert!(!t.is_light());
    }

    #[test]
    fn a_later_palette_line_wins() {
        let t = parse_ghostty("x", Source::User, "palette = 1=#000000\npalette = 1=#ff0000\n").unwrap();
        assert_eq!(t.palette, vec![(1, Rgb(255, 0, 0))]);
        assert_eq!(t.color(1), Some(Rgb(255, 0, 0)));
    }

    #[test]
    fn a_file_without_colours_is_not_a_theme() {
        assert_eq!(parse_ghostty("x", Source::User, "font-size = 12\n# background = #000000\n"), None);
        assert_eq!(parse_ghostty("x", Source::User, ""), None);
    }

    #[test]
    fn set_theme_args_follow_the_api() {
        let t = find_in(&builtin_themes(), "Solarized Light").cloned().unwrap();
        let args = t.set_theme_args();
        assert_eq!(args["background"], "#fdf6e3");
        assert_eq!(args["foreground"], "#657b83");
        assert_eq!(args["cursor"], "#657b83");
        assert_eq!(args["scheme"], "light");
        assert_eq!(args["palette"].as_array().unwrap().len(), 16);
        assert_eq!(args["palette"][4], "4=#268bd2");
        // Left-out colours go back to the terminal's own.
        let bare = parse_ghostty("x", Source::User, "palette = 1=#ff0000").unwrap();
        assert_eq!(bare.set_theme_args()["background"], "");
        assert_eq!(bare.set_theme_args()["scheme"], "dark");
    }

    #[test]
    fn has_the_built_in_themes() {
        let themes = builtin_themes();
        let names: Vec<&str> = themes.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names[0], DEFAULT_THEME);
        for name in ["Catppuccin Mocha", "Catppuccin Latte", "Dracula", "Gruvbox Dark", "Nord", "One Dark", "Solarized Dark", "Solarized Light", "Rosé Pine"] {
            assert!(names.contains(&name), "{name}");
        }
        let light: Vec<&str> = themes.iter().filter(|t| t.is_light()).map(|t| t.name.as_str()).collect();
        assert_eq!(light, ["Catppuccin Latte", "Solarized Light"]);
    }

    #[test]
    fn finds_themes_by_loose_name() {
        let themes = builtin_themes();
        assert_eq!(find_in(&themes, "dracula").unwrap().name, "Dracula");
        assert_eq!(find_in(&themes, "rose-pine").unwrap().name, "Rosé Pine");
        assert_eq!(find_in(&themes, "tokyonight").unwrap().name, "Tokyo Night");
        assert!(find_in(&themes, "nope").is_none());
        assert!(find_in(&themes, "--").is_none());
    }

    fn file(name: &str, source: Source, bg: &str) -> TermTheme {
        parse_ghostty(name, source, &format!("background = {bg}")).unwrap()
    }

    #[test]
    fn your_files_win_then_built_ins_then_ghosttys() {
        let files = vec![
            file("Zenburn", Source::System, "#3f3f3f"),
            file("Nord", Source::System, "#000000"),
            file("dracula", Source::User, "#111111"),
            file("Apple", Source::System, "#222222"),
            file("Apple", Source::User, "#333333"),
        ];
        let all = merge(builtin_themes(), files);
        let n = builtin_themes().len();
        assert_eq!(all.len(), n + 2);
        // Built-ins first, in their order; yours replaces one in place.
        assert_eq!(all[0].name, DEFAULT_THEME);
        let dracula = find_in(&all, "Dracula").unwrap();
        assert_eq!((dracula.source, dracula.background), (Source::User, Some(Rgb(0x11, 0x11, 0x11))));
        assert_eq!(find_in(&all, "Nord").unwrap().source, Source::Builtin);
        // Then the rest by name, yours over Ghostty's.
        assert_eq!(all[n].name, "Apple");
        assert_eq!(all[n].source, Source::User);
        assert_eq!(all[n + 1].name, "Zenburn");
    }

    #[test]
    fn reads_a_folder_of_themes() {
        let dir = temp_dir("themes");
        std::fs::write(dir.join("Mine"), MOCHA).unwrap();
        std::fs::write(dir.join("Empty"), "font-size = 3").unwrap();
        std::fs::write(dir.join(".hidden"), MOCHA).unwrap();
        let all = catalog_from(&[(dir.clone(), Source::User), (dir.join("missing"), Source::System)]);
        assert_eq!(all.len(), builtin_themes().len() + 1);
        assert_eq!(all.last().unwrap().name, "Mine");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn tokyo_night_keeps_the_original_chrome() {
        let t = find_in(&builtin_themes(), DEFAULT_THEME).cloned().unwrap();
        assert_eq!(derive_colors(&t), TOKYO_NIGHT_CHROME);
    }

    #[test]
    fn chrome_follows_the_theme() {
        for theme in builtin_themes() {
            let c = derive_colors(&theme);
            assert!(contrast(c.text, c.panel) >= 3.0, "{} text", theme.name);
            assert!(contrast(c.dim, c.panel) >= 2.5, "{} dim", theme.name);
            assert!(contrast(c.accent, c.panel) >= 2.0, "{} accent", theme.name);
            let panel_light = luminance(c.panel) > luminance(c.text);
            assert_eq!(panel_light, theme.is_light(), "{} is light", theme.name);
        }
        let latte = derive_colors(find_in(&builtin_themes(), "Catppuccin Latte").unwrap());
        assert_eq!(latte.panel, Rgb(0xef, 0xf1, 0xf5));
        assert_eq!(latte.text, Rgb(0x4c, 0x4f, 0x69));
        assert_eq!(latte.accent, Rgb(0x1e, 0x66, 0xf5));
        assert_eq!(latte.danger, Rgb(0xd2, 0x0f, 0x39));
        assert!(luminance(latte.bg) < luminance(latte.panel));
    }

    #[test]
    fn chrome_skips_colours_that_dont_read() {
        // A blue as dark as the background: the accent falls to bright blue.
        let t = parse_ghostty("x", Source::User, "background = #101010\nforeground = #f0f0f0\npalette = 4=#121212\npalette = 12=#5090ff").unwrap();
        assert_eq!(derive_colors(&t).accent, Rgb(0x50, 0x90, 0xff));
        // No colours at all still gives readable chrome.
        let none = parse_ghostty("x", Source::User, "palette = 200=#123456").unwrap();
        let c = derive_colors(&none);
        assert!(contrast(c.text, c.panel) >= 3.0);
    }

    #[test]
    fn mixes_and_measures() {
        assert_eq!(mix(Rgb(0, 0, 0), Rgb(255, 255, 255), 0.5), Rgb(128, 128, 128));
        assert_eq!(mix(Rgb(10, 20, 30), Rgb(0, 0, 0), 0.0), Rgb(10, 20, 30));
        assert!((contrast(Rgb(0, 0, 0), Rgb(255, 255, 255)) - 21.0).abs() < 0.01);
        assert_eq!(hex(Rgb(0x0a, 0xbc, 0xff)), "#0abcff");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openrex-theme-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn settings_round_trip_and_keep_other_keys() {
        let dir = temp_dir("settings");
        let file = SettingsFile::new(dir.join("openrex/settings.json"));
        assert_eq!(file.load().unwrap(), Saved::default());
        file.set("theme", json!("Nord")).unwrap();
        file.set("font_size", json!(15.0)).unwrap();
        std::fs::write(&file.path, r#"{"theme":"Nord","font_size":15,"font_family":"Iosevka","future":[1]}"#).unwrap();
        file.set("theme", json!("Dracula")).unwrap();
        let saved = file.load().unwrap();
        assert_eq!(saved, Saved { theme: Some("Dracula".into()), font_family: Some("Iosevka".into()), font_size: Some(15.0) });
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&file.path).unwrap()).unwrap();
        assert_eq!(raw["future"], json!([1]));
        // Nothing is left behind beside it.
        let names: Vec<_> = std::fs::read_dir(dir.join("openrex")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("settings.json")]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_broken_settings_file_is_left_alone() {
        let dir = temp_dir("broken");
        let file = SettingsFile::new(dir.join("settings.json"));
        std::fs::write(&file.path, "{ not json").unwrap();
        assert!(file.load().is_err());
        assert!(file.set("theme", json!("Nord")).is_err());
        assert_eq!(std::fs::read_to_string(&file.path).unwrap(), "{ not json");
        // Out-of-range and wrong-typed values read as unset.
        std::fs::write(&file.path, r#"{"font_size": 500, "theme": 3, "font_family": " "}"#).unwrap();
        assert_eq!(file.load().unwrap(), Saved::default());
        std::fs::remove_dir_all(dir).ok();
    }
}
