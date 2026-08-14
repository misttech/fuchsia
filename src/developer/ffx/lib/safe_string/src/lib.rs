// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::{Borrow, Cow};
use std::convert::AsRef;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt::{self, Debug, Display, Formatter};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Returns `true` if `c` is a control character that could affect terminal state.
///
/// This checks for both C0 (U+0000..=U+001F) and C1 (U+0080..=U+009F) control codes
/// as well as DEL (U+007F).
#[inline]
pub const fn is_control_character(c: char) -> bool {
    c.is_control()
}

/// Returns `true` if `s` contains any control characters.
#[inline]
pub fn contains_control_characters(s: &str) -> bool {
    s.chars().any(is_control_character)
}

/// An error indicating that a control character was found in a string when constructing
/// a [`SafeString`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlCharError {
    /// The byte index in the string where the first control character was encountered.
    pub byte_index: usize,
    /// The control character that was encountered.
    pub character: char,
}

impl ControlCharError {
    /// Returns the byte index where the control character occurred.
    pub fn byte_index(&self) -> usize {
        self.byte_index
    }

    /// Returns the control character that caused the failure.
    pub fn character(&self) -> char {
        self.character
    }
}

impl Display for ControlCharError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "string contains control character {:?} (U+{:04X}) at byte index {}",
            self.character, self.character as u32, self.byte_index
        )
    }
}

impl Error for ControlCharError {}

/// A safe string type that prevents terminal control injection attacks.
///
/// Terminal emulators interpret certain ASCII and Unicode control characters
/// (such as `ESC`, `CR`, `LF`, `BEL`, `BS`, and C1 controls) as commands to modify
/// terminal state. These control characters can change text formatting and colors,
/// move the cursor, overwrite earlier lines, clear the screen, or trigger terminal
/// operating system commands (OSC).
///
/// When untrusted strings (such as target names, device serial numbers, log messages,
/// or network metadata) are printed to the terminal, malicious or malformed inputs
/// containing control characters could alter terminal display or disguise output.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermSafe(String);

impl TermSafe {
    /// Constructs a `TermSafe` by replacing all control characters with ([`char::REPLACEMENT_CHARACTER`] / `\u{FFFD}`)
    pub fn from_str_lossy(s: impl AsRef<str>) -> Self {
        let redacted: String = s
            .as_ref()
            .chars()
            .map(|c| if is_control_character(c) { char::REPLACEMENT_CHARACTER } else { c })
            .collect();
        Self(redacted)
    }

    /// Constructs a `TermSafe` by escaping all control characters with [`char::escape_default`].
    pub fn from_str_escaped(s: impl AsRef<str>) -> Self {
        let s = s.as_ref();
        if !contains_control_characters(s) {
            return Self(s.to_owned());
        }
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if is_control_character(c) {
                out.extend(c.escape_default());
            } else {
                out.push(c);
            }
        }
        Self(out)
    }

    /// Returns a slice referencing the contained string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the contained string.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl Deref for TermSafe {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for TermSafe {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for TermSafe {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl AsRef<OsStr> for TermSafe {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(&self.0)
    }
}

impl AsRef<[u8]> for TermSafe {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl Borrow<str> for TermSafe {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl Display for TermSafe {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Debug for TermSafe {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TermSafe").field(&self.0).finish()
    }
}

impl From<TermSafe> for String {
    fn from(safe_string: TermSafe) -> Self {
        safe_string.0
    }
}

impl From<TermSafe> for PathBuf {
    fn from(safe_string: TermSafe) -> Self {
        PathBuf::from(safe_string.0)
    }
}

impl<'a> From<TermSafe> for Cow<'a, str> {
    fn from(safe_string: TermSafe) -> Self {
        Cow::Owned(safe_string.0)
    }
}

impl<'a> From<&'a TermSafe> for Cow<'a, str> {
    fn from(safe_string: &'a TermSafe) -> Self {
        Cow::Borrowed(safe_string.as_str())
    }
}

impl TryFrom<&str> for TermSafe {
    type Error = ControlCharError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        if let Some((byte_index, character)) =
            s.char_indices().find(|(_, c)| is_control_character(*c))
        {
            Err(ControlCharError { byte_index, character })
        } else {
            Ok(Self(s.to_owned()))
        }
    }
}

impl TryFrom<String> for TermSafe {
    type Error = ControlCharError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::try_from(s.as_str())
    }
}

impl FromStr for TermSafe {
    type Err = ControlCharError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl PartialEq<str> for TermSafe {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for TermSafe {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for TermSafe {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

impl PartialEq<TermSafe> for str {
    fn eq(&self, other: &TermSafe) -> bool {
        self == &other.0
    }
}

impl PartialEq<TermSafe> for &str {
    fn eq(&self, other: &TermSafe) -> bool {
        *self == &other.0
    }
}

impl PartialEq<TermSafe> for String {
    fn eq(&self, other: &TermSafe) -> bool {
        self == &other.0
    }
}

impl<'a> PartialEq<Cow<'a, str>> for TermSafe {
    fn eq(&self, other: &Cow<'a, str>) -> bool {
        self.0 == **other
    }
}

impl<'a> PartialEq<TermSafe> for Cow<'a, str> {
    fn eq(&self, other: &TermSafe) -> bool {
        **self == other.0
    }
}

impl Serialize for TermSafe {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TermSafe {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        TermSafe::try_from(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    #[test]
    fn test_accepts_clean_strings() {
        let safe = TermSafe::try_from("hello-world_123").unwrap();
        assert_eq!(safe.as_str(), "hello-world_123");
        assert_eq!(safe.len(), 15);
        assert!(!safe.is_empty());
        assert_eq!(safe, "hello-world_123");

        let empty = TermSafe::try_from("").unwrap();
        assert_eq!(empty.as_str(), "");
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());

        let unicode = TermSafe::try_from("こんにちは 🚀 世界").unwrap();
        assert_eq!(unicode.as_str(), "こんにちは 🚀 世界");
    }

    #[test]
    fn test_rejects_ascii_control_characters() {
        // ESC (\x1b)
        assert_matches!(
            TermSafe::try_from("hello\x1b[31mworld"),
            Err(ControlCharError { byte_index: 5, character: '\x1b' })
        );

        // NUL (\x00)
        assert_matches!(
            TermSafe::try_from("\x00prefix"),
            Err(ControlCharError { byte_index: 0, character: '\0' })
        );

        // BEL (\x07)
        assert_matches!(
            TermSafe::try_from("alert\x07"),
            Err(ControlCharError { byte_index: 5, character: '\x07' })
        );

        // BS (\x08)
        assert_matches!(
            TermSafe::try_from("back\x08space"),
            Err(ControlCharError { byte_index: 4, character: '\x08' })
        );

        // TAB (\t / \x09)
        assert_matches!(
            TermSafe::try_from("tab\tseparated"),
            Err(ControlCharError { byte_index: 3, character: '\t' })
        );

        // LF (\n / \x0a)
        assert_matches!(
            TermSafe::try_from("line1\nline2"),
            Err(ControlCharError { byte_index: 5, character: '\n' })
        );

        // CR (\r / \x0d)
        assert_matches!(
            TermSafe::try_from("line1\rline2"),
            Err(ControlCharError { byte_index: 5, character: '\r' })
        );

        // DEL (\x7f)
        assert_matches!(
            TermSafe::try_from("delete\x7fchar"),
            Err(ControlCharError { byte_index: 6, character: '\x7f' })
        );
    }

    #[test]
    fn test_rejects_c1_control_characters() {
        // C1 CSI (\u{009B})
        assert_matches!(
            TermSafe::try_from("c1_\u{009b}_control"),
            Err(ControlCharError { byte_index: 3, character: '\u{009B}' })
        );

        // C1 OSC (\u{009D})
        assert_matches!(
            TermSafe::try_from("c1_\u{009d}_osc"),
            Err(ControlCharError { byte_index: 3, character: '\u{009D}' })
        );

        // C1 boundary U+0080
        assert_matches!(
            TermSafe::try_from("c1_\u{0080}"),
            Err(ControlCharError { byte_index: 3, character: '\u{0080}' })
        );

        // C1 boundary U+009F
        assert_matches!(
            TermSafe::try_from("c1_\u{009f}"),
            Err(ControlCharError { byte_index: 3, character: '\u{009F}' })
        );
    }

    #[test]
    fn test_multi_byte_utf8_error_index() {
        // "こんにちは" is 15 bytes (3 bytes per character)
        let s = "こんにちは\x1b[31m";
        let err = TermSafe::try_from(s).unwrap_err();
        assert_eq!(err.byte_index(), 15);
        assert_eq!(err.character(), '\x1b');
    }

    #[test]
    fn test_from_str_lossy() {
        let s = "hello\x1b[31mworld\x07!";
        let safe = TermSafe::from_str_lossy(s);
        assert_eq!(
            safe.as_str(),
            &format!(
                "hello{}[31mworld{}!",
                char::REPLACEMENT_CHARACTER,
                char::REPLACEMENT_CHARACTER
            )
        );

        // String without control characters is unmodified
        let clean = "clean_string";
        assert_eq!(TermSafe::from_str_lossy(clean).as_str(), clean);
    }

    #[test]
    fn test_conversions_and_traits() {
        let safe = TermSafe::try_from("test string").unwrap();

        // Display
        assert_eq!(format!("{safe}"), "test string");

        // Debug
        assert_eq!(format!("{safe:?}"), "TermSafe(\"test string\")");

        // Deref
        assert!(safe.starts_with("test"));
        assert!(safe.ends_with("string"));
        assert_eq!(&safe[0..4], "test");

        // AsRef
        let s_ref: &str = safe.as_ref();
        assert_eq!(s_ref, "test string");

        // Borrow
        let borrowed: &str = safe.borrow();
        assert_eq!(borrowed, "test string");

        // From / Into String
        let owned: String = safe.clone().into();
        assert_eq!(owned, "test string");
        assert_eq!(safe.clone().into_inner(), "test string");

        // FromStr
        let parsed = "parsed_str".parse::<TermSafe>().unwrap();
        assert_eq!(parsed, "parsed_str");
        assert!("bad\nstr".parse::<TermSafe>().is_err());

        // TryFrom
        assert_eq!(TermSafe::try_from("good").unwrap(), "good");
        assert!(TermSafe::try_from("bad\0").is_err());
        assert_eq!(TermSafe::try_from(String::from("good_owned")).unwrap(), "good_owned");
        assert!(TermSafe::try_from(String::from("bad\x1b_owned")).is_err());

        // PartialEq comparisons
        let s = TermSafe::try_from("abc").unwrap();
        assert_eq!(s, "abc");
        assert_eq!("abc", s);
        assert_eq!(s, *"abc");
        assert_eq!(*"abc", s);
        assert_eq!(s, String::from("abc"));
        assert_eq!(String::from("abc"), s);

        // Default
        let default_safe = TermSafe::default();
        assert_eq!(default_safe, "");

        // Path and OsStr
        let path: &Path = safe.as_ref();
        assert_eq!(path, Path::new("test string"));
        let os_str: &OsStr = safe.as_ref();
        assert_eq!(os_str, OsStr::new("test string"));
        let bytes: &[u8] = safe.as_ref();
        assert_eq!(bytes, b"test string");
        let path_buf: PathBuf = safe.clone().into();
        assert_eq!(path_buf, PathBuf::from("test string"));

        // Cow
        let cow_owned: Cow<'_, str> = safe.clone().into();
        assert_eq!(cow_owned, Cow::Borrowed("test string"));
        let cow_borrowed: Cow<'_, str> = (&safe).into();
        assert_eq!(cow_borrowed, Cow::Borrowed("test string"));
        assert_eq!(safe, Cow::Borrowed("test string"));
        assert_eq!(Cow::Borrowed("test string"), safe);
    }

    #[test]
    fn test_from_str_escaped() {
        let s = "hello\x1b[31mworld\x07!\r\n";
        let safe = TermSafe::from_str_escaped(s);
        assert_eq!(safe.as_str(), "hello\\u{1b}[31mworld\\u{7}!\\r\\n");

        // String without control characters is unmodified
        let clean = "clean_string";
        assert_eq!(TermSafe::from_str_escaped(clean).as_str(), clean);
    }

    #[test]
    fn test_error_display() {
        let err = TermSafe::try_from("foo\x1bbar").unwrap_err();
        let display_msg = format!("{err}");
        assert!(display_msg.contains("byte index 3"));
        assert!(display_msg.contains("U+001B"));
    }

    #[test]
    fn test_serde_json() {
        let safe = TermSafe::try_from("serde_test").unwrap();
        let json = serde_json::to_string(&safe).unwrap();
        assert_eq!(json, "\"serde_test\"");

        let deserialized: TermSafe = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, safe);

        // Deserializing control characters should fail
        let bad_json = "\"hello\\u001bworld\"";
        let res: Result<TermSafe, _> = serde_json::from_str(bad_json);
        assert!(res.is_err());
    }

    #[test]
    fn test_is_control_and_contains_control() {
        assert!(is_control_character('\x1b'));
        assert!(is_control_character('\0'));
        assert!(is_control_character('\n'));
        assert!(!is_control_character('a'));
        assert!(!is_control_character(' '));
        assert!(!is_control_character('🦀'));

        assert!(contains_control_characters("hello\nworld"));
        assert!(!contains_control_characters("hello world 🦀"));
    }
}
