// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_uapi::signals::{SIGINT, SIGQUIT, SIGTSTP, Signal};
use starnix_uapi::{
    CREAD, CS8, CSIZE, ECHOCTL, EXTPROC, ICANON, IEXTEN, IUTF8, IXON, PARENB, VEOF, VEOL, VEOL2,
    VERASE, VINTR, VKILL, VQUIT, VSTART, VSTOP, VSUSP, VWERASE, cc_t, tcflag_t, uapi,
};

/// Number of columns per tab stop (`TABDLY` / `XTABS` and canonical tab erase).
pub(crate) const SPACES_PER_TAB: usize = 8;

/// Sentinel value indicating that a `c_cc` control character is disabled (`_POSIX_VDISABLE`).
pub(crate) const DISABLED_CHAR: u8 = 0;

/// ASCII backspace byte (`\b`).
pub(crate) const BACKSPACE_CHAR: u8 = 0x08;

/// ASCII DEL byte.
const DELETE_CHAR: u8 = 0x7f;

/// Maximum ASCII control character below space (`0x00..=0x1f`).
const MAX_ASCII_CONTROL: u8 = 0x1f;

/// Bitmask and tag for UTF-8 continuation bytes (`10xxxxxx`).
const UTF8_CONTINUATION_MASK: u8 = 0xc0;
const UTF8_CONTINUATION_TAG: u8 = 0x80;

/// Offset in ASCII between a control character and its printable caret letter (e.g.,
/// `b'C' - CONTROL_OFFSET == 0x03` for `^C`, and `0x7f ^ CONTROL_OFFSET == b'?'` for `^?`).
pub(crate) const CONTROL_OFFSET: u8 = 0x40;

const DEFAULT_SPEED: u32 = 38400;

/// Returns the ASCII byte for `c`. Panics if `c` is not an ASCII character.
const fn ascii_byte(c: char) -> u8 {
    assert!(c.is_ascii());
    c as u8
}

/// Returns the control character code associated with an uppercase ASCII letter (e.g., `'C'` -> 3).
const fn control_character(c: char) -> cc_t {
    ascii_byte(c) - ascii_byte('A') + 1
}

/// Returns the default `c_cc` control character table for a terminal.
const fn default_control_characters() -> [cc_t; 19] {
    [
        control_character('C'),  // VINTR = ^C
        control_character('\\'), // VQUIT = ^\
        DELETE_CHAR,             // VERASE = DEL
        control_character('U'),  // VKILL = ^U
        control_character('D'),  // VEOF = ^D
        0,                       // VTIME
        1,                       // VMIN
        0,                       // VSWTC
        control_character('Q'),  // VSTART = ^Q
        control_character('S'),  // VSTOP = ^S
        control_character('Z'),  // VSUSP = ^Z
        0,                       // VEOL
        control_character('R'),  // VREPRINT = ^R
        control_character('O'),  // VDISCARD = ^O
        control_character('W'),  // VWERASE = ^W
        control_character('V'),  // VLNEXT = ^V
        0,                       // VEOL2
        0,                       // Remaining entry in c_cc
        0,                       // Remaining entry in c_cc
    ]
}

/// Returns the default replica terminal configuration.
pub fn get_default_termios() -> uapi::termios2 {
    uapi::termios2 {
        c_iflag: uapi::ICRNL | uapi::IXON,
        c_oflag: uapi::OPOST | uapi::ONLCR,
        c_cflag: uapi::B38400 | uapi::CS8 | uapi::CREAD,
        c_lflag: uapi::ISIG
            | uapi::ICANON
            | uapi::ECHO
            | uapi::ECHOE
            | uapi::ECHOK
            | uapi::ECHOCTL
            | uapi::ECHOKE
            | uapi::IEXTEN,
        c_line: 0,
        c_cc: default_control_characters(),
        c_ispeed: DEFAULT_SPEED,
        c_ospeed: DEFAULT_SPEED,
    }
}

/// Returns `true` if `byte` is an ASCII control character (`0x00..=0x1f` or `0x7f`).
pub(crate) fn is_ascii_control(byte: u8) -> bool {
    byte <= MAX_ASCII_CONTROL || byte == DELETE_CHAR
}

/// Returns `true` if `byte` is treated as a word character by Linux `n_tty` (`VWERASE`),
/// matching ASCII alphanumerics, `'_'`, and non-punctuation Latin-1 / UTF-8 leading bytes.
pub(crate) fn is_word_erase_alnum(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || byte == b'_'
        || matches!(byte, 0xc0..=0xd6 | 0xd8..=0xf6 | 0xf8..=0xff)
}

/// Extension methods for inspecting and normalizing `uapi::termios2` configurations.
pub(crate) trait TermiosExt {
    fn has_input_flags(&self, flags: tcflag_t) -> bool;
    fn has_output_flags(&self, flags: tcflag_t) -> bool;
    fn has_local_flags(&self, flags: tcflag_t) -> bool;
    fn is_canon_enabled(&self) -> bool;
    fn is_extproc_enabled(&self) -> bool;
    fn normalize_pty_cflags(&mut self);
    fn is_standard_xon_xoff(&self) -> bool;
    fn is_cc(&self, index: u32, byte: u8) -> bool;
    fn is_iexten_cc(&self, index: u32, byte: u8) -> bool;
    fn is_eof(&self, byte: u8) -> bool;
    fn is_erase(&self, byte: u8) -> bool;
    fn is_werase(&self, byte: u8) -> bool;
    fn is_kill(&self, byte: u8) -> bool;
    fn is_eol(&self, byte: u8) -> bool;
    fn signal(&self, byte: u8) -> Option<Signal>;
    fn is_utf8_continuation(&self, byte: u8) -> bool;
    fn echoed_char_width(&self, byte: u8) -> usize;
}

impl TermiosExt for uapi::termios2 {
    fn has_input_flags(&self, flags: tcflag_t) -> bool {
        self.c_iflag & flags == flags
    }

    fn has_output_flags(&self, flags: tcflag_t) -> bool {
        self.c_oflag & flags == flags
    }

    fn has_local_flags(&self, flags: tcflag_t) -> bool {
        self.c_lflag & flags == flags
    }

    fn is_canon_enabled(&self) -> bool {
        self.has_local_flags(ICANON) && !self.is_extproc_enabled()
    }

    fn is_extproc_enabled(&self) -> bool {
        self.has_local_flags(EXTPROC)
    }

    fn normalize_pty_cflags(&mut self) {
        // Pseudo-terminals always force CS8 | CREAD and clear PARENB.
        self.c_cflag &= !(CSIZE | PARENB);
        self.c_cflag |= CS8 | CREAD;
    }

    fn is_standard_xon_xoff(&self) -> bool {
        self.has_input_flags(IXON)
            && self.c_cc[VSTOP as usize] == control_character('S')
            && self.c_cc[VSTART as usize] == control_character('Q')
    }

    fn is_cc(&self, index: u32, byte: u8) -> bool {
        byte != DISABLED_CHAR && self.c_cc[index as usize] == byte
    }

    fn is_iexten_cc(&self, index: u32, byte: u8) -> bool {
        self.has_local_flags(IEXTEN) && self.is_cc(index, byte)
    }

    fn is_eof(&self, byte: u8) -> bool {
        self.is_cc(VEOF, byte)
    }

    fn is_erase(&self, byte: u8) -> bool {
        self.is_cc(VERASE, byte)
    }

    fn is_werase(&self, byte: u8) -> bool {
        self.is_iexten_cc(VWERASE, byte)
    }

    fn is_kill(&self, byte: u8) -> bool {
        self.is_cc(VKILL, byte)
    }

    fn is_eol(&self, byte: u8) -> bool {
        self.is_cc(VEOL, byte) || self.is_iexten_cc(VEOL2, byte)
    }

    fn signal(&self, byte: u8) -> Option<Signal> {
        if self.is_cc(VINTR, byte) {
            Some(SIGINT)
        } else if self.is_cc(VQUIT, byte) {
            Some(SIGQUIT)
        } else if self.is_cc(VSUSP, byte) {
            Some(SIGTSTP)
        } else {
            None
        }
    }

    fn is_utf8_continuation(&self, byte: u8) -> bool {
        self.has_input_flags(IUTF8) && (byte & UTF8_CONTINUATION_MASK) == UTF8_CONTINUATION_TAG
    }

    fn echoed_char_width(&self, byte: u8) -> usize {
        if is_ascii_control(byte) {
            if self.has_local_flags(ECHOCTL) { 2 } else { 0 }
        } else if self.is_utf8_continuation(byte) {
            0
        } else {
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_ascii_and_control_character() {
        assert_eq!(ascii_byte(' '), 32);
        assert_eq!(control_character('C'), 3);
        assert_eq!(control_character('Q'), 17);
        assert_eq!(control_character('S'), 19);
    }

    #[fuchsia::test]
    fn test_signal_handling_with_disabled_chars() {
        let mut termios = get_default_termios();
        assert_eq!(termios.signal(3), Some(SIGINT));
        assert_eq!(termios.signal(28), Some(SIGQUIT));
        assert_eq!(termios.signal(26), Some(SIGTSTP));

        termios.c_cc[VINTR as usize] = DISABLED_CHAR;
        termios.c_cc[VQUIT as usize] = DISABLED_CHAR;
        termios.c_cc[VSUSP as usize] = DISABLED_CHAR;

        assert_eq!(termios.signal(0), None);
        assert_eq!(termios.signal(3), None);
        assert_eq!(termios.signal(28), None);
        assert_eq!(termios.signal(26), None);
    }

    #[fuchsia::test]
    fn test_normalize_pty_cflags() {
        let mut termios = get_default_termios();
        termios.c_cflag = uapi::CS5 | uapi::PARENB;
        termios.normalize_pty_cflags();
        assert_eq!(termios.c_cflag & CSIZE, CS8);
        assert_eq!(termios.c_cflag & CREAD, CREAD);
        assert_eq!(termios.c_cflag & PARENB, 0);
    }

    #[fuchsia::test]
    fn test_echoed_char_width_and_utf8() {
        let mut termios = get_default_termios();
        termios.c_iflag |= IUTF8;
        assert_eq!(termios.echoed_char_width(b'a'), 1);
        assert_eq!(termios.echoed_char_width(0x03), 2);
        assert_eq!(termios.echoed_char_width(0xa9), 0);

        termios.c_lflag &= !ECHOCTL;
        termios.c_iflag &= !IUTF8;
        assert_eq!(termios.echoed_char_width(0x03), 0);
        assert_eq!(termios.echoed_char_width(0xa9), 1);
    }
}
