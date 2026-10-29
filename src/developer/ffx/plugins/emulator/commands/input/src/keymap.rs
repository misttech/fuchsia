// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! QEMU key names and the US QWERTY mapping used by `text`.

/// The QMP `QKeyCode` names that can be sent, as listed in
/// <https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#enum-QMP-ui.QKeyCode>.
///
/// `unmapped` is left out because it doesn't name a key.
pub const QEMU_KEY_CODES: &[&str] = &[
    "shift",
    "shift_r",
    "alt",
    "alt_r",
    "ctrl",
    "ctrl_r",
    "menu",
    "esc",
    "1",
    "2",
    "3",
    "4",
    "5",
    "6",
    "7",
    "8",
    "9",
    "0",
    "minus",
    "equal",
    "backspace",
    "tab",
    "q",
    "w",
    "e",
    "r",
    "t",
    "y",
    "u",
    "i",
    "o",
    "p",
    "bracket_left",
    "bracket_right",
    "ret",
    "a",
    "s",
    "d",
    "f",
    "g",
    "h",
    "j",
    "k",
    "l",
    "semicolon",
    "apostrophe",
    "grave_accent",
    "backslash",
    "z",
    "x",
    "c",
    "v",
    "b",
    "n",
    "m",
    "comma",
    "dot",
    "slash",
    "asterisk",
    "spc",
    "caps_lock",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "num_lock",
    "scroll_lock",
    "kp_divide",
    "kp_multiply",
    "kp_subtract",
    "kp_add",
    "kp_enter",
    "kp_decimal",
    "sysrq",
    "kp_0",
    "kp_1",
    "kp_2",
    "kp_3",
    "kp_4",
    "kp_5",
    "kp_6",
    "kp_7",
    "kp_8",
    "kp_9",
    "less",
    "f11",
    "f12",
    "print",
    "home",
    "pgup",
    "pgdn",
    "end",
    "left",
    "up",
    "down",
    "right",
    "insert",
    "delete",
    "stop",
    "again",
    "props",
    "undo",
    "front",
    "copy",
    "open",
    "paste",
    "find",
    "cut",
    "lf",
    "help",
    "meta_l",
    "meta_r",
    "compose",
    "pause",
    "ro",
    "hiragana",
    "henkan",
    "yen",
    "muhenkan",
    "katakanahiragana",
    "kp_comma",
    "kp_equals",
    "power",
    "sleep",
    "wake",
    "audionext",
    "audioprev",
    "audiostop",
    "audioplay",
    "audiomute",
    "volumeup",
    "volumedown",
    "mediaselect",
    "mail",
    "calculator",
    "computer",
    "ac_home",
    "ac_back",
    "ac_forward",
    "ac_refresh",
    "ac_bookmarks",
    "lang1",
    "lang2",
    "f13",
    "f14",
    "f15",
    "f16",
    "f17",
    "f18",
    "f19",
    "f20",
    "f21",
    "f22",
    "f23",
    "f24",
];

/// Returns the [`QEMU_KEY_CODES`] entry equal to `name`.
fn find_key_code(name: &str) -> Option<&'static str> {
    QEMU_KEY_CODES.iter().find(|code| **code == name).copied()
}

/// Parses a combo such as `ctrl+alt+delete` into its QEMU key names, in order.
///
/// Names are case-sensitive and have no aliases. A key may appear only once in a combo.
pub fn parse_combo(combo: &str) -> Result<Vec<&'static str>, String> {
    let mut keys = Vec::new();
    for name in combo.split('+') {
        let Some(key) = find_key_code(name) else {
            return Err(format!(
                "unknown key name '{name}' in combo '{combo}'. Key names are lowercase QEMU \
                QKeyCode names, see `ffx emu input key --help`."
            ));
        };
        if keys.contains(&key) {
            return Err(format!("key '{name}' appears more than once in combo '{combo}'"));
        }
        keys.push(key);
    }
    Ok(keys)
}

/// A key that types a character.
#[derive(Debug, PartialEq)]
pub struct TypedKey {
    pub qcode: &'static str,
    /// True if shift must be held while the key is pressed.
    pub needs_shift: bool,
}

/// Maps a printable ASCII character to the key that types it on a US QWERTY keyboard.
///
/// Returns [`None`] for any other character.
pub fn ascii_to_key(c: char) -> Option<TypedKey> {
    let unshifted = |qcode| Some(TypedKey { qcode, needs_shift: false });
    let shifted = |qcode| Some(TypedKey { qcode, needs_shift: true });
    match c {
        'a'..='z' | '0'..='9' => unshifted(find_key_code(c.encode_utf8(&mut [0; 4]))?),
        'A'..='Z' => shifted(find_key_code(c.to_ascii_lowercase().encode_utf8(&mut [0; 4]))?),
        ' ' => unshifted("spc"),
        '!' => shifted("1"),
        '@' => shifted("2"),
        '#' => shifted("3"),
        '$' => shifted("4"),
        '%' => shifted("5"),
        '^' => shifted("6"),
        '&' => shifted("7"),
        '*' => shifted("8"),
        '(' => shifted("9"),
        ')' => shifted("0"),
        '-' => unshifted("minus"),
        '_' => shifted("minus"),
        '=' => unshifted("equal"),
        '+' => shifted("equal"),
        '[' => unshifted("bracket_left"),
        '{' => shifted("bracket_left"),
        ']' => unshifted("bracket_right"),
        '}' => shifted("bracket_right"),
        '\\' => unshifted("backslash"),
        '|' => shifted("backslash"),
        ';' => unshifted("semicolon"),
        ':' => shifted("semicolon"),
        '\'' => unshifted("apostrophe"),
        '"' => shifted("apostrophe"),
        '`' => unshifted("grave_accent"),
        '~' => shifted("grave_accent"),
        ',' => unshifted("comma"),
        '<' => shifted("comma"),
        '.' => unshifted("dot"),
        '>' => shifted("dot"),
        '/' => unshifted("slash"),
        '?' => shifted("slash"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_key_codes_unique() {
        let set: HashSet<_> = QEMU_KEY_CODES.iter().collect();
        assert_eq!(set.len(), QEMU_KEY_CODES.len());
        assert_eq!(find_key_code("unmapped"), None);
    }

    #[test]
    fn test_parse_combo() {
        assert_eq!(parse_combo("a").unwrap(), vec!["a"]);
        assert_eq!(parse_combo("ctrl+alt+delete").unwrap(), vec!["ctrl", "alt", "delete"]);
        assert_eq!(parse_combo("shift+f10").unwrap(), vec!["shift", "f10"]);
        assert_eq!(parse_combo("meta_l").unwrap(), vec!["meta_l"]);
        assert_eq!(parse_combo("kp_1").unwrap(), vec!["kp_1"]);
        assert_eq!(parse_combo("f24").unwrap(), vec!["f24"]);
    }

    #[test]
    fn test_parse_combo_errors() {
        for combo in ["", "ctrl+", "+a", "ctrl++a"] {
            let err = parse_combo(combo).unwrap_err();
            assert!(err.contains("unknown key name ''"), "{combo:?}: {err}");
        }
        assert!(parse_combo("a+a").unwrap_err().contains("more than once"));
        let err = parse_combo("ctrl+enter").unwrap_err();
        assert!(err.contains("unknown key name 'enter' in combo 'ctrl+enter'"), "{err}");
    }

    #[test]
    fn test_no_aliases() {
        for alias in ["enter", "return", "space", "control", "del", "escape", "KEYCODE_A", "A"] {
            assert!(parse_combo(alias).is_err(), "{alias} should be rejected");
        }
    }

    #[test]
    fn test_ascii_to_key_all_printable() {
        for c in (0x20u8..=0x7e).map(char::from) {
            let key = ascii_to_key(c).unwrap_or_else(|| panic!("no mapping for {c:?}"));
            assert_eq!(find_key_code(key.qcode), Some(key.qcode), "{c:?}");
        }
    }

    #[test]
    fn test_ascii_to_key_examples() {
        let key = |qcode, needs_shift| Some(TypedKey { qcode, needs_shift });
        assert_eq!(ascii_to_key('a'), key("a", false));
        assert_eq!(ascii_to_key('H'), key("h", true));
        assert_eq!(ascii_to_key('7'), key("7", false));
        assert_eq!(ascii_to_key('!'), key("1", true));
        assert_eq!(ascii_to_key(' '), key("spc", false));
        assert_eq!(ascii_to_key('?'), key("slash", true));
        assert_eq!(ascii_to_key('"'), key("apostrophe", true));
        assert_eq!(ascii_to_key('~'), key("grave_accent", true));
    }

    #[test]
    fn test_ascii_to_key_rejects() {
        for c in ['\n', '\t', '\r', '\x7f', '\0', 'é', '€', '日'] {
            assert_eq!(ascii_to_key(c), None, "{c:?}");
        }
    }
}
