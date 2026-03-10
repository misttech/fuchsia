// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// One or more valid path segments separated by forward slashes.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Path(String);

impl TryFrom<String> for Path {
    type Error = crate::ParseError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let () = validate_path(&value)?;
        Ok(Self(value))
    }
}

impl std::str::FromStr for Path {
    type Err = crate::ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let () = validate_path(s)?;
        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Display for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for Path {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for Path {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

// Succeeds if `path` is one or more valid path segments separated by forward slashes.
fn validate_path(path: &str) -> Result<(), crate::ParseError> {
    for s in path.split('/') {
        let () = crate::parse::validate_package_path_segment(s)
            .map_err(crate::ParseError::InvalidPathSegment)?;
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use assert_matches::assert_matches;

    macro_rules! test_err {
        (
            $(
                $test_name:ident => {
                    path = $path:expr,
                    err = $err:pat,
                }
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    assert_matches!(
                        validate_path($path),
                        Err($err)
                    );
                }
            )+
        }
    }

    test_err! {
        err_empty_path => {
            path = "",
            err = crate::ParseError::InvalidPathSegment(_),
        }
        err_leading_slash => {
            path = "/leading-slash",
            err = crate::ParseError::InvalidPathSegment(_),
        }
        err_trailing_slash => {
            path = "name/",
            err = crate::ParseError::InvalidPathSegment(_),
        }
        err_empty_segment => {
            path = "name//trailing",
            err = crate::ParseError::InvalidPathSegment(_),
        }
        err_invalid_segment => {
            path = "name/#/trailing",
            err = crate::ParseError::InvalidPathSegment(_),
        }
    }

    #[test]
    fn success() {
        for path in ["name", "name/other", "name/other/more"] {
            let () = validate_path(path).unwrap();
        }
    }
}
