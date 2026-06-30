// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Data structures and functions relevant to `fuchsia.io` name processing.
//!
//! These names may be used to designate the location of a node as it
//! appears in a directory.
//!
//! These should be aligned with the library comments in sdk/fidl/fuchsia.io/io.fidl.

use fidl_fuchsia_io as fio;
use static_assertions::const_assert_eq;
use std::borrow::Borrow;
use std::fmt::Display;
use std::ops::Deref;
use thiserror::Error;
use zx_status::Status;

/// The type for the name of a node, i.e. a single path component, e.g. `foo`.
///
/// ## Invariants
///
/// A valid node name must meet the following criteria:
///
/// * It cannot be longer than [MAX_NAME_LENGTH].
/// * It cannot be empty.
/// * It cannot be ".." (dot-dot).
/// * It cannot be "." (single dot).
/// * It cannot contain "/".
/// * It cannot contain embedded NUL.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Name(Box<str>);

/// The maximum length, in bytes, of a single filesystem component.
pub const MAX_NAME_LENGTH: usize = fio::MAX_NAME_LENGTH as usize;
const_assert_eq!(MAX_NAME_LENGTH as u64, fio::MAX_NAME_LENGTH);

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum ParseNameError {
    #[error("name is too long")]
    TooLong,

    #[error("name cannot be empty")]
    Empty,

    #[error("name cannot be `.`")]
    Dot,

    #[error("name cannot be `..`")]
    DotDot,

    #[error("name cannot contain `/`")]
    Slash,

    #[error("name cannot contain embedded NUL")]
    EmbeddedNul,
}

impl From<ParseNameError> for Status {
    fn from(value: ParseNameError) -> Self {
        match value {
            ParseNameError::TooLong => Status::BAD_PATH,
            _ => Status::INVALID_ARGS,
        }
    }
}

impl Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validates whether a string slice is a valid node name.
///
/// A valid node name must meet the following criteria:
/// * It cannot be longer than [MAX_NAME_LENGTH] (255 bytes).
/// * It cannot be empty.
/// * It cannot be "." (single dot) or ".." (dot-dot).
/// * It cannot contain "/" (slash) or embedded NUL (`\0`) characters.
pub fn validate_name(name: &str) -> Result<(), ParseNameError> {
    let len = name.len();
    if len > MAX_NAME_LENGTH {
        return Err(ParseNameError::TooLong);
    }
    if len == 0 {
        return Err(ParseNameError::Empty);
    }
    let bytes = name.as_bytes();
    if bytes[0] == b'.' {
        if len == 1 {
            return Err(ParseNameError::Dot);
        }
        if len == 2 && bytes[1] == b'.' {
            return Err(ParseNameError::DotDot);
        }
    }
    if let Some(idx) = memchr::memchr2(b'/', 0, name.as_bytes()) {
        if name.as_bytes()[idx] == 0 {
            return Err(ParseNameError::EmbeddedNul);
        } else {
            return Err(ParseNameError::Slash);
        }
    }
    Ok(())
}

impl From<Name> for String {
    fn from(value: Name) -> Self {
        value.0.into_string()
    }
}

impl TryFrom<String> for Name {
    type Error = ParseNameError;

    fn try_from(value: String) -> Result<Name, ParseNameError> {
        validate_name(&value)?;
        Ok(Name(value.into_boxed_str()))
    }
}

impl Deref for Name {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
        &*self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    #[test]
    fn test_validate_name() {
        assert_matches!(validate_name(&"a".repeat(1000)), Err(ParseNameError::TooLong));
        assert_matches!(
            validate_name(
                std::str::from_utf8(&vec![65; fio::MAX_NAME_LENGTH as usize + 1]).unwrap()
            ),
            Err(ParseNameError::TooLong)
        );
        assert_matches!(
            validate_name(std::str::from_utf8(&vec![65; fio::MAX_NAME_LENGTH as usize]).unwrap()),
            Ok(())
        );
        assert_matches!(validate_name(""), Err(ParseNameError::Empty));
        assert_matches!(validate_name("."), Err(ParseNameError::Dot));
        assert_matches!(validate_name(".."), Err(ParseNameError::DotDot));
        assert_matches!(validate_name(".a"), Ok(()));
        assert_matches!(validate_name("..a"), Ok(()));
        assert_matches!(validate_name("a/b"), Err(ParseNameError::Slash));
        assert_matches!(validate_name("a\0b"), Err(ParseNameError::EmbeddedNul));
        assert_matches!(validate_name("abc"), Ok(()));
    }

    #[test]
    fn test_try_from() {
        assert_matches!(Name::try_from("a".repeat(1000)), Err(ParseNameError::TooLong));
        assert_matches!(Name::try_from("abc".to_string()), Ok(Name(name)) if &*name == "abc");
    }

    #[test]
    fn test_into() {
        let name = Name::try_from("a".to_string()).unwrap();
        let name: String = name.into();
        assert_eq!(name, "a".to_string());
    }

    #[test]
    fn test_deref() {
        let name = Name::try_from("a".to_string()).unwrap();
        let name: &str = &name;
        assert_eq!(name, "a");
    }
}
