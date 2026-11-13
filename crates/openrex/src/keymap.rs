//! Key bindings, the way Rex describes them.
//!
//! OpenRex tells the server which actions it can perform and which keys it
//! binds by default. The server merges those with the configuration file
//! (`init.lua`) and answers with the bindings to use. This module reads keys
//! written in Rex's spelling, such as `ctrl+shift+d` or `ctrl+[KeyD]`,
//! matches key presses against them, and follows key sequences and modes.
//!
//! Action names follow Rex.app's, so one configuration file works with both.

use std::collections::HashMap;

use gpui::Keystroke;
use serde_json::{Value, json};

pub const SHIFT: u8 = 1;
pub const CTRL: u8 = 2;
pub const ALT: u8 = 4;
pub const SUPER: u8 = 8;

/// One key of a binding: modifiers and the key they go with.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub mods: u8,
    pub base: Base,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Base {
    /// The key that types this character without shift, in the current
    /// keyboard layout.
    Char(char),
    /// A key that types nothing, such as `enter` or `left`.
    Named(String),
    /// A physical key, named by its W3C `KeyboardEvent.code`, such as `KeyA`.
    Code(String),
}

/// A key that was pressed, described every way a binding might name it.
#[derive(Clone, Debug, PartialEq)]
pub struct Press {
    pub mods: u8,
    /// The character the key types without shift, lowercased.
    pub ch: Option<char>,
    /// The name of a key that types nothing, in Rex's spelling.
    pub named: Option<String>,
    /// The physical key, by its W3C code.
    pub code: Option<String>,
}

const NAMED: &[&str] = &[
    "enter", "escape", "tab", "space", "backspace", "delete", "insert", "left", "right", "up", "down", "home",
    "end", "pageup", "pagedown", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
    "f13", "f14", "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24", "menu",
];

fn normalize_named(name: &str) -> Option<&'static str> {
    let n = match name {
        "return" => "enter",
        "esc" => "escape",
        "page_up" | "pgup" => "pageup",
        "page_down" | "pgdn" => "pagedown",
        "del" => "delete",
        "arrowleft" => "left",
        "arrowright" => "right",
        "arrowup" => "up",
        "arrowdown" => "down",
        other => other,
    };
    NAMED.iter().copied().find(|k| *k == n)
}

/// Parses a key as Rex writes it, such as `shift+cmd+d`, `Ctrl+Alt+Left`,
/// `ctrl+[KeyA]`, or `ctrl+shift+=`.
pub fn parse_key(spec: &str) -> Option<Key> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    // The key itself may be "+", as in "ctrl++", which Rex doesn't allow but
    // costs nothing to read.
    let (mods_part, key_part) = match spec.rfind('+') {
        Some(i) if i + 1 == spec.len() && i > 0 => (&spec[..i - 1], "+"),
        Some(i) => (&spec[..i], &spec[i + 1..]),
        None => ("", spec),
    };
    let mut mods = 0;
    for m in mods_part.split('+').filter(|m| !m.is_empty()) {
        mods |= match m.to_ascii_lowercase().as_str() {
            "shift" => SHIFT,
            "ctrl" | "control" => CTRL,
            "alt" | "opt" | "option" => ALT,
            "cmd" | "command" | "super" | "meta" | "win" => SUPER,
            _ => return None,
        };
    }
    let base = if let Some(code) = key_part.strip_prefix('[').and_then(|k| k.strip_suffix(']')) {
        Base::Code(code.to_string())
    } else {
        let lower = key_part.to_lowercase();
        let mut chars = lower.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Base::Char(c),
            _ => Base::Named(normalize_named(&lower)?.to_string()),
        }
    };
    Some(Key { mods, base })
}

impl Key {
    pub fn matches(&self, p: &Press) -> bool {
        if self.mods != p.mods {
            return false;
        }
        match &self.base {
            Base::Char(c) => p.ch == Some(*c) || (*c == ' ' && p.named.as_deref() == Some("space")),
            Base::Named(n) => p.named.as_deref() == Some(n.as_str()),
            Base::Code(code) => p.code.as_deref() == Some(code.as_str()),
        }
    }

    /// The key in Rex's spelling, for showing next to an action.
    pub fn spelled(&self) -> String {
        let mut out = mods_prefix(self.mods);
        match &self.base {
            Base::Char(c) => out.push(*c),
            Base::Named(n) => out.push_str(n),
            Base::Code(code) => {
                out.push('[');
                out.push_str(code);
                out.push(']');
            }
        }
        out
    }

    /// The key as people read it: `Ctrl+Shift+D`.
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        for (bit, name) in [(CTRL, "Ctrl"), (ALT, "Alt"), (SHIFT, "Shift"), (SUPER, "Super")] {
            if self.mods & bit != 0 {
                parts.push(name.to_string());
            }
        }
        parts.push(match &self.base {
            Base::Char(c) => c.to_uppercase().to_string(),
            Base::Named(n) => match n.as_str() {
                "pageup" => "PgUp".into(),
                "pagedown" => "PgDn".into(),
                n => {
                    let mut s = n.to_string();
                    if let Some(f) = s.get_mut(0..1) {
                        f.make_ascii_uppercase();
                    }
                    s
                }
            },
            Base::Code(code) => code.strip_prefix("Key").or(code.strip_prefix("Digit")).unwrap_or(code).to_string(),
        });
        parts.join("+")
    }
}

fn mods_prefix(mods: u8) -> String {
    let mut out = String::new();
    for (bit, name) in [(SHIFT, "shift+"), (CTRL, "ctrl+"), (ALT, "alt+"), (SUPER, "super+")] {
        if mods & bit != 0 {
            out.push_str(name);
        }
    }
    out
}

/// The symbols a US keyboard types with shift, and the key under each.
const SHIFTED: &[(char, char)] = &[
    ('!', '1'), ('@', '2'), ('#', '3'), ('$', '4'), ('%', '5'), ('^', '6'), ('&', '7'),
    ('*', '8'), ('(', '9'), (')', '0'), ('_', '-'), ('+', '='), ('{', '['), ('}', ']'),
    ('|', '\\'), (':', ';'), ('"', '\''), ('<', ','), ('>', '.'), ('?', '/'), ('~', '`'),
];

fn code_of(base: char) -> Option<String> {
    Some(match base {
        'a'..='z' => format!("Key{}", base.to_ascii_uppercase()),
        '0'..='9' => format!("Digit{base}"),
        '-' => "Minus".into(),
        '=' => "Equal".into(),
        '[' => "BracketLeft".into(),
        ']' => "BracketRight".into(),
        '\\' => "Backslash".into(),
        ';' => "Semicolon".into(),
        '\'' => "Quote".into(),
        ',' => "Comma".into(),
        '.' => "Period".into(),
        '/' => "Slash".into(),
        '`' => "Backquote".into(),
        ' ' => "Space".into(),
        _ => return None,
    })
}

fn code_of_named(name: &str) -> Option<String> {
    Some(match name {
        "enter" => "Enter".into(),
        "escape" => "Escape".into(),
        "tab" => "Tab".into(),
        "space" => "Space".into(),
        "backspace" => "Backspace".into(),
        "delete" => "Delete".into(),
        "insert" => "Insert".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "pageup" => "PageUp".into(),
        "pagedown" => "PageDown".into(),
        "left" => "ArrowLeft".into(),
        "right" => "ArrowRight".into(),
        "up" => "ArrowUp".into(),
        "down" => "ArrowDown".into(),
        f if f.starts_with('f') && f[1..].parse::<u8>().is_ok() => f.to_uppercase(),
        _ => return None,
    })
}

/// Describes a key press for matching. `None` for a press of a modifier
/// alone.
pub fn press_of(ks: &Keystroke) -> Option<Press> {
    let m = &ks.modifiers;
    let mut mods = 0;
    if m.shift {
        mods |= SHIFT;
    }
    if m.control {
        mods |= CTRL;
    }
    if m.alt {
        mods |= ALT;
    }
    if m.platform {
        mods |= SUPER;
    }
    let key = ks.key.as_str();
    if matches!(key, "shift" | "control" | "alt" | "platform" | "function" | "") {
        return None;
    }
    if let Some(named) = normalize_named(key) {
        if named == "space" {
            return Some(Press { mods, ch: Some(' '), named: Some("space".into()), code: Some("Space".into()) });
        }
        return Some(Press { mods, ch: None, named: Some(named.to_string()), code: code_of_named(named) });
    }
    let mut chars = key.chars();
    let c = match (chars.next(), chars.next()) {
        (Some(c), None) => c,
        _ => return None,
    };
    let base = match SHIFTED.iter().find(|(s, _)| *s == c) {
        Some((_, base)) => {
            // gpui reports a shifted symbol without shift.
            mods |= SHIFT;
            *base
        }
        None => c.to_lowercase().next().unwrap_or(c),
    };
    Some(Press { mods, ch: Some(base), named: None, code: code_of(base) })
}

impl Press {
    /// The press in Rex's spelling, as `client.key` takes it in `prefix`.
    pub fn spelled(&self) -> String {
        let mut out = mods_prefix(self.mods);
        match (&self.named, self.ch) {
            (Some(n), _) => out.push_str(n),
            (None, Some(c)) => out.push(c),
            (None, None) => out.push_str(self.code.as_deref().unwrap_or("")),
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub keys: Vec<Key>,
    pub action: String,
    pub args: Value,
    pub mode: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mode {
    pub name: String,
    pub exclusive: bool,
    pub blocked: Vec<Vec<Key>>,
}

/// An action anything can perform: OpenRex itself, the configuration file,
/// or a block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActionInfo {
    pub name: String,
    pub kind: String,
    pub title: String,
    pub description: String,
    pub category: String,
    pub keywords: String,
    pub args: Option<Value>,
}

impl ActionInfo {
    fn from_json(v: &Value) -> Option<ActionInfo> {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Some(ActionInfo {
            name: v["name"].as_str()?.to_string(),
            kind: s("kind"),
            title: s("title"),
            description: s("description"),
            category: s("category"),
            keywords: s("keywords"),
            args: v.get("args").filter(|a| !a.is_null()).cloned(),
        })
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "name": self.name,
            "kind": self.kind,
            "title": self.title,
            "category": self.category,
        });
        if !self.description.is_empty() {
            v["description"] = json!(self.description);
        }
        if !self.keywords.is_empty() {
            v["keywords"] = json!(self.keywords);
        }
        if let Some(args) = &self.args {
            v["args"] = args.clone();
        }
        v
    }

    /// Whether the action takes arguments a person must give, so the palette
    /// can't run it as it is.
    pub fn needs_args(&self) -> bool {
        self.args.as_ref().is_some_and(|a| a["required"].as_array().is_some_and(|r| !r.is_empty()))
    }
}

/// What a key press does.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolution {
    /// Perform this binding's action.
    Run(Binding),
    /// The press begins or continues a key sequence. Wait for the next key.
    Pending,
    /// No binding takes the key. Send it to the terminal.
    Unbound,
    /// Drop the key: an exclusive mode doesn't bind it, or it broke a
    /// sequence.
    Swallow,
}

/// The keymap the server resolved for this client.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    pub bindings: Vec<Binding>,
    pub modes: HashMap<String, Mode>,
    /// The configuration file's actions and the blocks' actions.
    pub server_actions: Vec<ActionInfo>,
    pub diagnostics: Vec<String>,
    pub config_path: String,
    pub config_exists: bool,
    pub hash: String,
    pub instance_id: String,
}

fn parse_keys(v: &Value) -> Option<Vec<Key>> {
    v.as_array()?.iter().map(|k| parse_key(k.as_str()?)).collect()
}

impl Keymap {
    /// Reads a `client.keymap` result.
    pub fn from_result(v: &Value) -> Keymap {
        let bindings = v["bindings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| {
                Some(Binding {
                    keys: parse_keys(&b["keys"]).filter(|k| !k.is_empty())?,
                    action: b["action"].as_str()?.to_string(),
                    args: b.get("args").cloned().unwrap_or(Value::Null),
                    mode: b["mode"].as_str().filter(|m| !m.is_empty()).map(str::to_string),
                    source: b["source"].as_str().unwrap_or("").to_string(),
                })
            })
            .collect();
        let modes = v["modes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let name = m["name"].as_str()?.to_string();
                let blocked = m["blocked"].as_array().into_iter().flatten().filter_map(parse_keys).collect();
                Some((name.clone(), Mode { name, exclusive: m["exclusive"].as_bool().unwrap_or(false), blocked }))
            })
            .collect();
        let server_actions = v["actions"].as_array().into_iter().flatten().filter_map(ActionInfo::from_json).collect();
        let diagnostics = v["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| {
                let source = d["source"].as_str().map(|s| format!("{s}: ")).unwrap_or_default();
                format!("{}{}{}", d["severity"].as_str().unwrap_or("warning"), if source.is_empty() { ": " } else { " " }, source)
                    + d["message"].as_str().unwrap_or("")
            })
            .collect();
        let config = &v["config"];
        Keymap {
            bindings,
            modes,
            server_actions,
            diagnostics,
            config_path: config["path"].as_str().unwrap_or("").to_string(),
            config_exists: config["exists"].as_bool().unwrap_or(false),
            hash: config["hash"].as_str().unwrap_or("").to_string(),
            instance_id: config["instance_id"].as_str().unwrap_or("").to_string(),
        }
    }

    /// The keymap to use before the server answers: the defaults alone.
    pub fn defaults() -> Keymap {
        let bindings = DEFAULT_BINDINGS
            .iter()
            .filter_map(|(key, action, args)| {
                Some(Binding {
                    keys: key.split('>').map(parse_key).collect::<Option<Vec<_>>>()?,
                    action: action.to_string(),
                    args: if args.is_empty() { Value::Null } else { serde_json::from_str(args).ok()? },
                    mode: None,
                    source: "default".into(),
                })
            })
            .collect();
        Keymap { bindings, ..Default::default() }
    }

    /// Decides what a press does, given the active modes (innermost last),
    /// the keys of an unfinished sequence, and which actions can run.
    pub fn resolve(&self, modes: &[String], prefix: &[Press], press: &Press, available: impl Fn(&str) -> bool) -> Resolution {
        let seq: Vec<&Press> = prefix.iter().chain(std::iter::once(press)).collect();
        let try_mode = |mode: Option<&str>| {
            self.bindings
                .iter()
                .filter(|b| b.mode.as_deref() == mode && available(&b.action))
                .find(|b| b.keys.len() >= seq.len() && b.keys.iter().zip(&seq).all(|(k, p)| k.matches(p)))
                .map(|b| if b.keys.len() == seq.len() { Resolution::Run(b.clone()) } else { Resolution::Pending })
        };
        if let Some(active) = modes.last() {
            if let Some(r) = try_mode(Some(active)) {
                return r;
            }
            if let Some(mode) = self.modes.get(active) {
                if mode.exclusive {
                    return Resolution::Swallow;
                }
                let blocked = mode.blocked.iter().any(|keys| keys.len() == seq.len() && keys.iter().zip(&seq).all(|(k, p)| k.matches(p)));
                if blocked {
                    return if prefix.is_empty() { Resolution::Unbound } else { Resolution::Swallow };
                }
            }
        }
        if let Some(r) = try_mode(None) {
            return r;
        }
        if prefix.is_empty() { Resolution::Unbound } else { Resolution::Swallow }
    }

    /// The keys bound to an action outside every mode, for showing in menus.
    pub fn keys_for(&self, action: &str, args: &Value) -> Option<String> {
        self.bindings
            .iter()
            .find(|b| b.mode.is_none() && b.action == action && (args.is_null() || b.args == *args))
            .map(|b| b.keys.iter().map(Key::display).collect::<Vec<_>>().join(" "))
    }
}

/// Builds the `key` object of `client.key` or a `key` block event from a
/// key written in Rex's spelling, for `pane.send_key`.
pub fn key_event_of(key: &Key) -> Value {
    let mut mods = Vec::new();
    for (bit, name) in [(SHIFT, "shift"), (CTRL, "ctrl"), (ALT, "alt"), (SUPER, "super")] {
        if key.mods & bit != 0 {
            mods.push(name);
        }
    }
    let typing = key.mods & (CTRL | SUPER) == 0;
    let (code, text, unshifted) = match &key.base {
        Base::Char(c) => {
            let text = if !typing {
                String::new()
            } else if key.mods & SHIFT != 0 {
                shifted(*c).to_string()
            } else {
                c.to_string()
            };
            (code_of(*c), text, *c as u32)
        }
        Base::Named(n) => {
            let text = match n.as_str() {
                "space" if typing => " ".to_string(),
                _ => String::new(),
            };
            (code_of_named(n), text, if n == "space" { 32 } else { 0 })
        }
        Base::Code(c) => (Some(c.clone()), String::new(), 0),
    };
    let key_name = code.map(|c| w3c_to_rex(&c)).unwrap_or_else(|| "unidentified".into());
    json!({
        "key": key_name,
        "mods": mods.join("+"),
        "consumed_mods": if key.mods & SHIFT != 0 && !text.is_empty() { "shift" } else { "" },
        "text": text,
        "unshifted_codepoint": unshifted,
        "composing": false,
        "action": "press",
    })
}

fn shifted(c: char) -> char {
    if c.is_ascii_lowercase() {
        return c.to_ascii_uppercase();
    }
    SHIFTED.iter().find(|(_, b)| *b == c).map(|(s, _)| *s).unwrap_or(c)
}

/// `KeyA` → `key_a`, `ArrowLeft` → `arrow_left`: the server's spelling of a
/// W3C code.
fn w3c_to_rex(code: &str) -> String {
    let mut out = String::new();
    for (i, ch) in code.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        if ch.is_ascii_digit() && i > 0 && !out.ends_with('_') && !out.chars().last().is_some_and(|c| c.is_ascii_digit()) {
            // Digit1 → digit_1, F12 stays f12.
            if !code.starts_with('F') {
                out.push('_');
            }
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// OpenRex's default keys. A binding's keys may be a sequence, written
/// `ctrl+b>c`. Arguments are JSON.
pub const DEFAULT_BINDINGS: &[(&str, &str, &str)] = &[
    ("ctrl+shift+p", "client.palette.toggle", ""),
    ("ctrl+shift+n", "session.new", ""),
    ("ctrl+shift+k", "session.switch", ""),
    ("ctrl+shift+o", "pane.go_to", ""),
    ("ctrl+shift+t", "client.tab.new", ""),
    ("ctrl+tab", "client.tab.next", ""),
    ("ctrl+pagedown", "client.tab.next", ""),
    ("ctrl+shift+tab", "client.tab.previous", ""),
    ("ctrl+pageup", "client.tab.previous", ""),
    ("ctrl+shift+pagedown", "client.tab.move.forward", ""),
    ("ctrl+shift+pageup", "client.tab.move.backward", ""),
    ("alt+[Digit1]", "client.tab.goto", r#"{"index":1}"#),
    ("alt+[Digit2]", "client.tab.goto", r#"{"index":2}"#),
    ("alt+[Digit3]", "client.tab.goto", r#"{"index":3}"#),
    ("alt+[Digit4]", "client.tab.goto", r#"{"index":4}"#),
    ("alt+[Digit5]", "client.tab.goto", r#"{"index":5}"#),
    ("alt+[Digit6]", "client.tab.goto", r#"{"index":6}"#),
    ("alt+[Digit7]", "client.tab.goto", r#"{"index":7}"#),
    ("alt+[Digit8]", "client.tab.goto", r#"{"index":8}"#),
    ("alt+[Digit9]", "client.tab.goto", r#"{"index":9}"#),
    ("ctrl+shift+d", "pane.split.right", ""),
    ("ctrl+shift+e", "pane.split.down", ""),
    ("ctrl+shift+w", "pane.close", ""),
    ("ctrl+shift+enter", "pane.zoom", ""),
    ("ctrl+alt+enter", "pane.float", ""),
    ("ctrl+alt+m", "pane.minimize_floating", ""),
    ("ctrl+alt+shift+enter", "pane.minimize_all_floating", ""),
    ("ctrl+alt+left", "pane.focus.left", ""),
    ("ctrl+alt+right", "pane.focus.right", ""),
    ("ctrl+alt+up", "pane.focus.up", ""),
    ("ctrl+alt+down", "pane.focus.down", ""),
    ("ctrl+alt+shift+left", "pane.move.left", ""),
    ("ctrl+alt+shift+right", "pane.move.right", ""),
    ("ctrl+alt+shift+up", "pane.move.up", ""),
    ("ctrl+alt+shift+down", "pane.move.down", ""),
    ("ctrl+shift+c", "pane.copy", ""),
    ("ctrl+shift+v", "pane.paste", ""),
    ("ctrl+shift+a", "pane.select_all", ""),
    ("shift+pageup", "client.scroll.page_up", ""),
    ("shift+pagedown", "client.scroll.page_down", ""),
    ("ctrl+shift+up", "client.prompt.previous", ""),
    ("ctrl+shift+down", "client.prompt.next", ""),
    ("ctrl+shift+f", "client.find.open", ""),
    ("ctrl+shift+g", "pane.go_to_directory", ""),
    ("ctrl+=", "client.font.increase", ""),
    ("ctrl+shift+=", "client.font.increase", ""),
    ("ctrl+-", "client.font.decrease", ""),
    ("ctrl+0", "client.font.reset", ""),
    ("ctrl+shift+b", "client.sidebar.toggle", ""),
    ("ctrl+shift+[Period]", "client.sidebar.activity.toggle", ""),
    ("ctrl+shift+r", "client.reconnect", ""),
    ("ctrl+shift+q", "client.quit", ""),
];

fn dir_args(values: &[&str]) -> Option<Value> {
    Some(json!({
        "type": "object",
        "properties": {"direction": {"type": "string", "enum": values}},
        "required": ["direction"],
    }))
}

fn opt_args(props: Value) -> Option<Value> {
    Some(json!({"type": "object", "properties": props}))
}

/// Every action OpenRex performs itself, declared to the server so that key
/// bindings, `rex do`, and other clients can name them.
pub fn client_actions() -> Vec<ActionInfo> {
    let a = |name: &str, title: &str, category: &str, keywords: &str, description: &str, args: Option<Value>| ActionInfo {
        name: name.into(),
        kind: "client".into(),
        title: title.into(),
        description: description.into(),
        category: category.into(),
        keywords: keywords.into(),
        args,
    };
    let dirs4 = ["left", "right", "up", "down"];
    let label = || opt_args(json!({"label": {"type": "string"}}));
    vec![
        // App
        a("client.palette.toggle", "Command Palette…", "App", "command palette search commands actions", "", None),
        a("client.diagnostics", "Rex Diagnostics", "App", "diagnostics debug server keymap config problems", "Shows the server, the keymap's problems, and the clients.", None),
        a("client.version.copy", "Copy Version Info", "App", "version build copy clipboard about", "Copies the app and server versions to the clipboard.", None),
        a("client.debug.copy", "Copy Debug Info", "App", "debug info report copy clipboard", "Copies a report of the app, server, and session state.", None),
        a("client.notice.dismiss", "Dismiss Notice", "App", "dismiss notice error banner hide", "", None),
        a("client.config.copy_path", "Copy Config File Path", "App", "config configuration init.lua path copy", "", None),
        a("client.config.reload", "Reload Config", "App", "config configuration reload init.lua keybindings", "", None),
        a("client.scripts.reload", "Reload OpenRex Scripts", "App", "lua scripts reload client openrex", "Loads ~/.config/openrex/scripts/*.lua again.", None),
        a("client.reconnect", "Reconnect", "App", "reconnect connection server retry", "Connects to the server again.", None),
        a("client.quit", "Quit OpenRex", "App", "quit exit close app", "The server keeps every session running.", None),
        a("client.close", "Close", "Window", "close pane tab", "Closes the focused pane, else the selected tab.", None),
        a("client.server.status", "Server Status", "App", "server status version identity tailscale whoami", "", None),
        a("client.clients", "Connected Clients", "App", "clients connected list who", "", None),
        a("client.tailscale.enable", "Enable Tailscale", "App", "tailscale remote access tailnet enable", "Lets devices on your tailnet reach the server.", None),
        a("client.tailscale.disable", "Disable Tailscale", "App", "tailscale remote access tailnet disable", "", None),
        a("client.tailscale.logout", "Sign Out of Tailscale", "App", "tailscale logout sign out", "Signs the server out and turns built-in Tailscale off.", None),
        a("client.tailscale.rename", "Rename on Tailscale…", "App", "tailscale rename hostname device name", "", opt_args(json!({"hostname": {"type": "string"}}))),
        a("client.set_label", "Set Client Label…", "App", "label name client rex -C address", "Names this client so rex -C NAME can address it.", opt_args(json!({"label": {"type": "string"}}))),
        a("client.other.perform", "Run Action on Another Client…", "App", "client perform action remote other app control", "Asks another connected client, such as Rex.app, to perform one of its actions.", opt_args(json!({"client": {"type": "string"}, "action": {"type": "string"}}))),
        a("session.clients", "Clients in This Session", "Session", "clients attached session who", "", None),
        a("client.tab.new_with", "New Tab With…", "Tab", "new tab block kind flavor creator type", "Opens a tab with any kind of block the server makes.", opt_args(json!({"flavor": {"type": "string"}}))),
        a("pane.new_detached", "New Detached Pane", "Pane", "new detached background pane block hidden", "Starts a shell that runs without a place in any tab.", None),
        a("pane.lower_floating", "Send Floating Pane to Back", "Pane", "floating lower back behind send", "", None),
        a("pane.methods", "Pane Methods…", "Pane", "methods block api call describe inspect advanced", "Lists every method the focused block offers and runs one.", opt_args(json!({"block": {"type": "string"}, "method": {"type": "string"}}))),
        a("client.tailscale.login", "Sign In to Tailscale…", "App", "tailscale login sign in", "Copies a sign-in link to the clipboard.", None),
        // Session
        a("session.new", "New Session", "Session", "create session add", "", None),
        a("session.switch", "Change Session…", "Session", "session switch jump go", "", None),
        a("session.select", "Go to Session", "Session", "session select show jump go identifier id", "", Some(json!({"type": "object", "properties": {"session": {"type": "string"}}, "required": ["session"]}))),
        a("session.next", "Next Session", "Session", "cycle session forward", "", None),
        a("session.previous", "Previous Session", "Session", "cycle session back", "", None),
        a("session.rename", "Rename Session", "Session", "rename session name title", "", label()),
        a("session.close", "Close Session", "Session", "close kill destroy delete session", "", opt_args(json!({"session": {"type": "string"}, "confirm": {"type": "boolean"}}))),
        a("session.refresh", "Refresh Sessions", "Session", "reload sync sessions", "", None),
        a("session.move.up", "Move Session Up", "Session", "move session reorder up earlier", "", None),
        a("session.move.down", "Move Session Down", "Session", "move session reorder down later", "", None),
        // Host
        a("client.host.switch", "Switch Host", "Host", "host remote machine server switch go connect", "", opt_args(json!({"host": {"type": "string"}}))),
        a("client.host.add", "Add Remote Host…", "Host", "host remote add server machine url connect", "", opt_args(json!({"address": {"type": "string"}, "label": {"type": "string"}}))),
        a("client.host.remove", "Remove Host…", "Host", "remove forget delete host remote server", "", opt_args(json!({"host": {"type": "string"}}))),
        // Tab
        a("client.tab.new", "New Tab", "Tab", "create tab window add", "", None),
        a("client.tab.close", "Close Tab", "Tab", "close tab window", "", None),
        a("client.tab.close_others", "Close Other Tabs", "Tab", "close other tabs windows kill only keep", "", None),
        a("client.tab.rename", "Rename Tab", "Tab", "rename tab window name title", "", label()),
        a("client.tab.next", "Show Next Tab", "Tab", "cycle tab window forward", "", None),
        a("client.tab.previous", "Show Previous Tab", "Tab", "cycle tab window back", "", None),
        a("client.tab.goto", "Go to Tab", "Tab", "tab window select number index jump go", "", Some(json!({"type": "object", "properties": {"index": {"type": "integer", "minimum": 1}}, "required": ["index"]}))),
        a("client.tab.move", "Move Tab", "Tab", "move tab window reorder", "", Some(json!({"type": "object", "properties": {"index": {"type": "integer", "minimum": 1}}, "required": ["index"]}))),
        a("client.tab.move.backward", "Move Tab Backward", "Tab", "move tab window reorder left earlier", "", None),
        a("client.tab.move.forward", "Move Tab Forward", "Tab", "move tab window reorder right later", "", None),
        // Pane
        a("pane.split", "Split Pane", "Pane", "pane split terminal divide new", "", dir_args(&["left", "right", "up", "down", "auto"])),
        a("pane.split.left", "Split Pane Left", "Pane", "pane split terminal divide new left", "", None),
        a("pane.split.right", "Split Pane Right", "Pane", "pane split terminal divide new right", "", None),
        a("pane.split.up", "Split Pane Up", "Pane", "pane split terminal divide new above up", "", None),
        a("pane.split.down", "Split Pane Down", "Pane", "pane split terminal divide new below down", "", None),
        a("pane.split.auto", "Split Pane Best Fit", "Pane", "pane split terminal divide new automatic best fit", "", None),
        a("pane.focus", "Focus Pane", "Pane", "pane focus navigate", "", dir_args(&dirs4)),
        a("pane.focus.left", "Focus Pane Left", "Pane", "pane focus navigate left", "", None),
        a("pane.focus.right", "Focus Pane Right", "Pane", "pane focus navigate right", "", None),
        a("pane.focus.up", "Focus Pane Up", "Pane", "pane focus navigate up above", "", None),
        a("pane.focus.down", "Focus Pane Down", "Pane", "pane focus navigate down below", "", None),
        a("pane.focus_next", "Focus Next Pane", "Pane", "pane focus next forward", "", None),
        a("pane.focus_previous", "Focus Previous Pane", "Pane", "pane focus previous back", "", None),
        a("pane.move", "Move Pane", "Pane", "pane move swap rearrange", "", dir_args(&dirs4)),
        a("pane.move.left", "Move Pane Left", "Pane", "pane move swap rearrange left", "", None),
        a("pane.move.right", "Move Pane Right", "Pane", "pane move swap rearrange right", "", None),
        a("pane.move.up", "Move Pane Up", "Pane", "pane move swap rearrange up above", "", None),
        a("pane.move.down", "Move Pane Down", "Pane", "pane move swap rearrange down below", "", None),
        a("pane.float", "Float Pane", "Pane", "pane float floating window dock undock overlay pop out tile", "Floats the focused pane, or docks a floating one back into the layout.", None),
        a("pane.new_floating", "New Floating Pane", "Pane", "pane new create float floating window terminal overlay popup scratchpad", "", None),
        a("pane.minimize_floating", "Minimize Floating Window", "Pane", "pane floating window hide show toggle minimize restore", "", None),
        a("pane.minimize_all_floating", "Minimize Floating Windows", "Pane", "pane floating windows all hide show toggle minimize restore", "", None),
        a("pane.zoom", "Zoom Pane", "Pane", "pane zoom maximize restore terminal", "", None),
        a("pane.close", "Close Pane", "Pane", "pane close remove terminal", "", None),
        a("pane.close_others", "Close Other Panes", "Pane", "close other panes kill only keep focused", "", None),
        a("pane.balance", "Balance Splits", "Pane", "pane split balance equalize evenly distribute resize layout", "", None),
        a("pane.resize", "Resize Pane", "Pane", "pane resize grow shrink divider wider narrower taller shorter", "", Some(json!({"type": "object", "properties": {"direction": {"type": "string", "enum": dirs4}, "amount": {"type": "integer", "minimum": 1}}, "required": ["direction"]}))),
        a("pane.move_to_new_tab", "Move Pane to New Tab", "Pane", "pane block move new window tab detach split", "", None),
        a("pane.move_to_tab", "Move Pane to Tab…", "Pane", "pane block move tab window send other existing join", "", opt_args(json!({"index": {"type": "integer", "minimum": 1}}))),
        a("pane.detach", "Detach Pane", "Pane", "pane block detach hide background keep running", "Takes the pane out of its tab. It keeps running.", None),
        a("pane.attach", "Show Detached Pane…", "Pane", "pane block attach detached restore place background", "", opt_args(json!({"block": {"type": "string"}}))),
        a("pane.go_to_directory", "Go to Directory…", "Pane", "go to directory folder path cd new tab open navigate", "Opens a new tab in a directory.", opt_args(json!({"path": {"type": "string"}}))),
        a("pane.go_to", "Go to Anything…", "Pane", "go to anything pane block tab session jump switch navigate find search", "", None),
        a("pane.view_activity", "View Activity…", "Pane", "view activity active working blocked failed done agents programs status", "", None),
        a("pane.insert_path", "Insert Path…", "Pane", "insert path file folder directory cursor type paste complete browse", "", opt_args(json!({"path": {"type": "string"}}))),
        a("pane.copy_id", "Copy Pane ID", "Pane", "pane block id identifier copy clipboard", "Copies the focused pane's block ID to the clipboard.", None),
        a("pane.rename", "Rename Pane", "Pane", "rename pane block name title label", "", label()),
        a("pane.paste", "Paste into Pane", "Pane", "paste clipboard insert text terminal", "", None),
        a("pane.copy", "Copy from Pane", "Pane", "copy selection clipboard text terminal", "", None),
        a("pane.select_all", "Select All in Pane", "Pane", "select all selection text terminal scrollback", "", None),
        a("pane.send_key", "Send Key", "Pane", "send key terminal press chord binding", "Sends one key to the focused terminal.", Some(json!({"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}))),
        a("pane.clear", "Clear Terminal", "Pane", "clear screen scrollback terminal", "", None),
        a("pane.copy_text", "Copy Scrollback as Text", "Pane", "copy all screen scrollback history text clipboard format", "Copies the screen and scrollback, with wrapped lines joined.", None),
        a("pane.copy_html", "Copy Scrollback as HTML", "Pane", "copy all screen scrollback html colors styles clipboard format", "Copies the screen and scrollback as HTML with their colors.", None),
        a("pane.save_scrollback", "Save Scrollback…", "Pane", "save export write scrollback history file text", "Saves the screen and scrollback to a text file in ~/Downloads.", opt_args(json!({"path": {"type": "string"}}))),
        a("pane.info", "Pane Info", "Pane", "info process program pid directory size owner modes echo input details inspect", "Shows what the terminal runs and how it is set up.", None),
        a("pane.reset", "Reset Terminal", "Pane", "reset terminal state", "", None),
        a("pane.download", "Download File…", "Pane", "download file save copy from server remote", "Saves a file from the terminal's machine to ~/Downloads.", opt_args(json!({"path": {"type": "string"}}))),
        a("pane.upload", "Upload Files…", "Pane", "upload file send copy to server remote", "Sends local files to the terminal's machine and types their paths.", opt_args(json!({"paths": {"type": "array", "items": {"type": "string"}}}))),
        a("client.key.forward", "Send Key to Server", "Pane", "send key forward server script lua binding", "Sends the key that performed it to the focused terminal, for a script to answer.", None),
        a("client.prompt.previous", "Jump to Previous Prompt", "Pane", "jump scroll previous prompt command mark shell integration up", "", None),
        a("client.prompt.next", "Jump to Next Prompt", "Pane", "jump scroll next prompt command mark shell integration down", "", None),
        a("client.scroll.page_up", "Scroll Page Up", "Pane", "scroll page up back scrollback history", "", None),
        a("client.scroll.page_down", "Scroll Page Down", "Pane", "scroll page down forward scrollback history", "", None),
        a("client.scroll.top", "Scroll to Top", "Pane", "scroll top start beginning scrollback history", "", None),
        a("client.scroll.bottom", "Scroll to Bottom", "Pane", "scroll bottom end latest", "", None),
        // Keys
        a("client.mode.enter", "Enter Mode", "Keys", "mode enter key table prefix resize modal", "Makes the keys a mode binds perform, until the mode is left.", Some(json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}))),
        a("client.mode.exit", "Exit Mode", "Keys", "mode exit leave quit key table escape", "", None),
        a("client.mode.exit_all", "Exit All Modes", "Keys", "mode exit leave quit all key table escape", "", None),
        // Find
        a("client.find.open", "Find…", "Find", "find search text scrollback highlight", "", opt_args(json!({"query": {"type": "string"}}))),
        a("client.find.next", "Find Next", "Find", "find search next forward match", "", None),
        a("client.find.previous", "Find Previous", "Find", "find search previous back match", "", None),
        a("client.find.close", "Close Find", "Find", "find search close hide", "", None),
        // View
        a("client.font.increase", "Increase Font Size", "View", "terminal font size larger bigger increase zoom text", "", None),
        a("client.font.decrease", "Decrease Font Size", "View", "terminal font size smaller decrease zoom text", "", None),
        a("client.font.reset", "Reset Font Size", "View", "terminal font size reset default original text", "", None),
        a("client.sidebar.toggle", "Toggle Sidebar", "View", "sidebar collapse expand fold hide show sessions", "", None),
        a("client.sidebar.activity.toggle", "Toggle Activity", "View", "activity mode view filter working waiting attention programs status sidebar", "", None),
        a("client.theme.change", "Change Theme…", "View", "theme colour color scheme palette appearance", "", opt_args(json!({"theme": {"type": "string"}}))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn ks(key: &str, ctrl: bool, shift: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers { control: ctrl, shift, ..Default::default() },
            key: key.into(),
            key_char: None,
        }
    }

    #[test]
    fn parses_rex_key_spellings() {
        assert_eq!(parse_key("Ctrl+Shift+D"), Some(Key { mods: CTRL | SHIFT, base: Base::Char('d') }));
        assert_eq!(parse_key("shift+cmd+d"), Some(Key { mods: SHIFT | SUPER, base: Base::Char('d') }));
        assert_eq!(parse_key("ctrl+[KeyA]"), Some(Key { mods: CTRL, base: Base::Code("KeyA".into()) }));
        assert_eq!(parse_key("alt+PageUp"), Some(Key { mods: ALT, base: Base::Named("pageup".into()) }));
        assert_eq!(parse_key("ctrl+shift+="), Some(Key { mods: CTRL | SHIFT, base: Base::Char('=') }));
        assert_eq!(parse_key("hyper+x"), None);
        assert_eq!(parse_key("ctrl+nonsense"), None);
    }

    #[test]
    fn matches_presses_on_unshifted_keys_and_physical_codes() {
        let plus = press_of(&ks("+", true, false)).unwrap();
        assert!(parse_key("ctrl+shift+=").unwrap().matches(&plus));
        let a = press_of(&ks("a", true, false)).unwrap();
        assert!(parse_key("ctrl+a").unwrap().matches(&a));
        assert!(parse_key("ctrl+[KeyA]").unwrap().matches(&a));
        assert!(!parse_key("ctrl+shift+a").unwrap().matches(&a));
        let left = press_of(&ks("left", true, false)).unwrap();
        assert!(parse_key("ctrl+left").unwrap().matches(&left));
        assert!(parse_key("ctrl+[ArrowLeft]").unwrap().matches(&left));
        assert_eq!(a.spelled(), "ctrl+a");
    }

    fn keymap(json: Value) -> Keymap {
        Keymap::from_result(&json)
    }

    #[test]
    fn resolves_bindings_sequences_and_modes() {
        let km = keymap(json!({
            "bindings": [
                {"keys": ["ctrl+b", "c"], "action": "client.tab.new"},
                {"keys": ["ctrl+d"], "action": "unknown.action"},
                {"keys": ["ctrl+d"], "action": "pane.split", "args": {"direction": "right"}},
                {"keys": ["h"], "action": "pane.resize", "args": {"direction": "left"}, "mode": "resize"},
                {"keys": ["escape"], "action": "client.mode.exit", "mode": "resize"},
            ],
            "modes": [{"name": "resize", "exclusive": true}],
            "diagnostics": [], "config": {"path": "/x/init.lua", "exists": false, "generation": 1, "instance_id": "i", "hash": ""}
        }));
        let available = |a: &str| a != "unknown.action";
        let p = |k: &str, ctrl: bool| press_of(&ks(k, ctrl, false)).unwrap();
        // The first available binding on a key wins.
        match km.resolve(&[], &[], &p("d", true), available) {
            Resolution::Run(b) => assert_eq!((b.action.as_str(), b.args["direction"].as_str()), ("pane.split", Some("right"))),
            other => panic!("{other:?}"),
        }
        assert_eq!(km.resolve(&[], &[], &p("b", true), available), Resolution::Pending);
        assert!(matches!(km.resolve(&[], &[p("b", true)], &p("c", false), available), Resolution::Run(b) if b.action == "client.tab.new"));
        assert_eq!(km.resolve(&[], &[p("b", true)], &p("x", false), available), Resolution::Swallow);
        assert_eq!(km.resolve(&[], &[], &p("x", false), available), Resolution::Unbound);
        // An exclusive mode swallows the keys it doesn't bind.
        let modes = vec!["resize".to_string()];
        assert!(matches!(km.resolve(&modes, &[], &p("h", false), available), Resolution::Run(b) if b.action == "pane.resize"));
        assert_eq!(km.resolve(&modes, &[], &p("x", false), available), Resolution::Swallow);
        assert_eq!(km.config_path, "/x/init.lua");
    }

    #[test]
    fn defaults_parse_and_describe_their_keys() {
        let km = Keymap::defaults();
        assert_eq!(km.bindings.len(), DEFAULT_BINDINGS.len());
        assert_eq!(km.keys_for("pane.split.right", &Value::Null).as_deref(), Some("Ctrl+Shift+D"));
        assert_eq!(km.keys_for("client.tab.goto", &json!({"index": 2})).as_deref(), Some("Alt+2"));
        // Every default names a declared action.
        let names: Vec<String> = client_actions().into_iter().map(|a| a.name).collect();
        for (_, action, _) in DEFAULT_BINDINGS {
            assert!(names.iter().any(|n| n == action), "{action} isn't declared");
        }
    }

    #[test]
    fn builds_key_events_for_send_key() {
        let ev = key_event_of(&parse_key("ctrl+l").unwrap());
        assert_eq!((ev["key"].as_str(), ev["mods"].as_str(), ev["text"].as_str()), (Some("key_l"), Some("ctrl"), Some("")));
        let ev = key_event_of(&parse_key("shift+a").unwrap());
        assert_eq!((ev["text"].as_str(), ev["consumed_mods"].as_str()), (Some("A"), Some("shift")));
        assert_eq!(key_event_of(&parse_key("left").unwrap())["key"], "arrow_left");
        assert_eq!(key_event_of(&parse_key("1").unwrap())["key"], "digit_1");
        assert_eq!(key_event_of(&parse_key("f5").unwrap())["key"], "f5");
    }
}
