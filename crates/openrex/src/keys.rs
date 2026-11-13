//! Turns a key press into the key event the Rex server takes in `client.key`.
//!
//! The server encodes the key for the program in the terminal, so the client
//! only describes what was pressed: the physical key, the modifiers, and the
//! text it typed.

use gpui::{Keystroke, Modifiers};
use serde_json::{Value, json};

/// The symbols a US keyboard types with shift, and the key under each.
const SHIFTED: &[(char, char)] = &[
    ('!', '1'),
    ('@', '2'),
    ('#', '3'),
    ('$', '4'),
    ('%', '5'),
    ('^', '6'),
    ('&', '7'),
    ('*', '8'),
    ('(', '9'),
    (')', '0'),
    ('_', '-'),
    ('+', '='),
    ('{', '['),
    ('}', ']'),
    ('|', '\\'),
    (':', ';'),
    ('"', '\''),
    ('<', ','),
    ('>', '.'),
    ('?', '/'),
    ('~', '`'),
];

fn physical(base: char) -> Option<String> {
    Some(match base {
        'a'..='z' => format!("key_{base}"),
        '0'..='9' => format!("digit_{base}"),
        '-' => "minus".into(),
        '=' => "equal".into(),
        '[' => "bracket_left".into(),
        ']' => "bracket_right".into(),
        '\\' => "backslash".into(),
        ';' => "semicolon".into(),
        '\'' => "quote".into(),
        ',' => "comma".into(),
        '.' => "period".into(),
        '/' => "slash".into(),
        '`' => "backquote".into(),
        _ => return None,
    })
}

fn named(key: &str) -> Option<&'static str> {
    Some(match key {
        "enter" => "enter",
        "tab" => "tab",
        "backspace" => "backspace",
        "escape" => "escape",
        "space" => "space",
        "delete" => "delete",
        "insert" => "insert",
        "home" => "home",
        "end" => "end",
        "pageup" => "page_up",
        "pagedown" => "page_down",
        "left" => "arrow_left",
        "right" => "arrow_right",
        "up" => "arrow_up",
        "down" => "arrow_down",
        "f1" => "f1",
        "f2" => "f2",
        "f3" => "f3",
        "f4" => "f4",
        "f5" => "f5",
        "f6" => "f6",
        "f7" => "f7",
        "f8" => "f8",
        "f9" => "f9",
        "f10" => "f10",
        "f11" => "f11",
        "f12" => "f12",
        "f13" => "f13",
        "f14" => "f14",
        "f15" => "f15",
        "f16" => "f16",
        "f17" => "f17",
        "f18" => "f18",
        "f19" => "f19",
        "f20" => "f20",
        "f21" => "f21",
        "f22" => "f22",
        "f23" => "f23",
        "f24" => "f24",
        "menu" => "context_menu",
        _ => return None,
    })
}

/// The held modifiers, written the way the server reads them.
pub fn mods_string(m: &Modifiers) -> String {
    let mut mods = Vec::new();
    if m.shift {
        mods.push("shift");
    }
    if m.control {
        mods.push("ctrl");
    }
    if m.alt {
        mods.push("alt");
    }
    if m.platform {
        mods.push("super");
    }
    mods.join("+")
}

/// The `key` object of `client.key` for a key press, or `None` for a press
/// that types nothing and names no key the server knows.
pub fn rex_key(ks: &Keystroke, is_repeat: bool) -> Option<Value> {
    let m = &ks.modifiers;
    let mut shift = m.shift;
    let mut consumed_shift = false;
    let mut unshifted: u32 = 0;

    let mut chars = ks.key.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };

    let key = if let Some(name) = named(&ks.key) {
        if name == "space" {
            unshifted = ' ' as u32;
        }
        name.to_string()
    } else if let Some(c) = single {
        let base = match SHIFTED.iter().find(|(s, _)| *s == c) {
            Some((_, base)) => {
                // gpui drops shift from the modifiers of a shifted symbol.
                shift = true;
                consumed_shift = true;
                *base
            }
            None => c.to_ascii_lowercase(),
        };
        if !base.is_control() {
            unshifted = base as u32;
        }
        physical(base).unwrap_or_else(|| "unidentified".into())
    } else {
        "unidentified".to_string()
    };

    // With ctrl or super held, the key is a command, not text.
    let text = match &ks.key_char {
        Some(t) if !m.control && !m.platform => t.clone(),
        _ => String::new(),
    };
    if key == "unidentified" && text.is_empty() {
        return None;
    }
    if shift && !text.is_empty() && text.chars().any(|c| c.is_uppercase()) {
        consumed_shift = true;
    }

    let mods = mods_string(&Modifiers { shift, ..*m });

    Some(json!({
        "key": key,
        "mods": mods,
        "consumed_mods": if consumed_shift && !text.is_empty() { "shift" } else { "" },
        "text": text,
        "unshifted_codepoint": unshifted,
        "composing": false,
        "action": if is_repeat { "repeat" } else { "press" },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(key: &str, key_char: Option<&str>, m: Modifiers) -> Keystroke {
        Keystroke { modifiers: m, key: key.into(), key_char: key_char.map(str::to_string) }
    }

    #[test]
    fn plain_letter() {
        let k = rex_key(&ks("n", Some("n"), Modifiers::default()), false).unwrap();
        assert_eq!(k["key"], "key_n");
        assert_eq!(k["text"], "n");
        assert_eq!(k["mods"], "");
        assert_eq!(k["unshifted_codepoint"], 110);
        assert_eq!(k["action"], "press");
    }

    #[test]
    fn capital_letter_consumes_shift() {
        let m = Modifiers { shift: true, ..Default::default() };
        let k = rex_key(&ks("n", Some("N"), m), true).unwrap();
        assert_eq!((k["mods"].as_str(), k["consumed_mods"].as_str()), (Some("shift"), Some("shift")));
        assert_eq!(k["text"], "N");
        assert_eq!(k["action"], "repeat");
    }

    #[test]
    fn control_key_sends_no_text() {
        let m = Modifiers { control: true, ..Default::default() };
        let k = rex_key(&ks("g", None, m), false).unwrap();
        assert_eq!(
            k,
            json!({"key": "key_g", "mods": "ctrl", "consumed_mods": "", "text": "",
                   "unshifted_codepoint": 103, "composing": false, "action": "press"})
        );
    }

    #[test]
    fn shifted_symbol_restores_shift() {
        let k = rex_key(&ks("!", Some("!"), Modifiers::default()), false).unwrap();
        assert_eq!(k["key"], "digit_1");
        assert_eq!(k["mods"], "shift");
        assert_eq!(k["consumed_mods"], "shift");
        assert_eq!(k["unshifted_codepoint"], '1' as u32);
    }

    #[test]
    fn named_keys_and_modifiers() {
        let m = Modifiers { shift: true, alt: true, ..Default::default() };
        let k = rex_key(&ks("left", None, m), false).unwrap();
        assert_eq!(k["key"], "arrow_left");
        assert_eq!(k["mods"], "shift+alt");
        assert_eq!(k["text"], "");
        let k = rex_key(&ks("space", Some(" "), Modifiers::default()), false).unwrap();
        assert_eq!((k["key"].as_str(), k["text"].as_str()), (Some("space"), Some(" ")));
        let k = rex_key(&ks("pagedown", None, Modifiers::default()), false).unwrap();
        assert_eq!(k["key"], "page_down");
    }

    #[test]
    fn layout_text_without_a_known_key_still_types() {
        let k = rex_key(&ks("sterling", Some("£"), Modifiers::default()), false).unwrap();
        assert_eq!(k["key"], "unidentified");
        assert_eq!(k["text"], "£");
        assert!(rex_key(&ks("xf86audiomute", None, Modifiers::default()), false).is_none());
    }
}
