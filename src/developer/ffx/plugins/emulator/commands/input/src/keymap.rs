// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! QEMU key names.

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
}
