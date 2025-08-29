// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, bail};
use camino::Utf8PathBuf;
use regex::Regex;

use std::fs;
use std::sync::OnceLock;

// A helper function to get the compiled regex, describing a valid string
// that can be used when communicating with the upstream versioning service.
// The regex is initialized only on the first call.
fn valid_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9./_-]{0,63}$").unwrap())
}

/// Return an error of the given string does not satisfy the string constraints
/// in upstream versioning servers.
pub fn validate_string_for_upstream_versioning(candidate: String) -> Result<String> {
    if valid_regex().is_match(&candidate) {
        Ok(candidate)
    } else {
        bail!(
            "Invalid version string: \"{}\". The string must be 1-63 characters long, start with an alphanumeric character, and only contain alphanumeric characters, '.', '_', '-', or '/'.",
            candidate
        )
    }
}

/// Return the "version" string if it is provided.
/// If not, return the contents of the file located at the path "version_file".
/// If neither argument is provided, return the string "unversioned".
pub fn get_release_version(
    version: &Option<String>,
    version_file: &Option<Utf8PathBuf>,
) -> Result<String> {
    get_string_or_file_content(
        version,
        version_file,
        "unversioned",
        "version and version_file cannot both be supplied",
    )
}

/// Return the "repo" string if it is provided.
/// If not, return the contents of the file located at the path "repo_file".
/// If neither argument is provided, return the string "unknown".
pub fn get_release_repository(
    repo: &Option<String>,
    repo_file: &Option<Utf8PathBuf>,
) -> Result<String> {
    get_string_or_file_content(
        repo,
        repo_file,
        "unknown",
        "repo and repo_file cannot both be supplied",
    )
}

fn get_string_or_file_content(
    field: &Option<String>,
    field_file: &Option<Utf8PathBuf>,
    undefined_message: &str,
    both_defined_message: &str,
) -> Result<String> {
    let s = match (field, field_file) {
        (None, None) => undefined_message.to_string(),
        (Some(_), Some(_)) => bail!(both_defined_message.to_string()),
        (Some(field), _) => field.to_string(),
        (None, Some(field_file)) => {
            let s = fs::read_to_string(field_file)?;
            s.trim().to_string()
        }
    };
    if &s == "" {
        return Ok(undefined_message.to_string());
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;
    use std::io::Write;
    use tempfile;

    #[test]
    fn test_default() {
        let version: Option<String> = None;
        let version_file: Option<Utf8PathBuf> = None;
        let version = get_string_or_file_content(
            &version,
            &version_file,
            "unversioned",
            "version and version_file cannot both be supplied",
        )
        .unwrap();
        assert_eq!("unversioned".to_string(), version);
    }

    #[test]
    fn test_version_string() {
        let version: Option<String> = Some("version_string".to_string());
        let version_file: Option<Utf8PathBuf> = None;
        let version = get_string_or_file_content(
            &version,
            &version_file,
            "unversioned",
            "version and version_file cannot both be supplied",
        )
        .unwrap();
        assert_eq!("version_string".to_string(), version);
    }

    #[test]
    fn test_empty_version_string() {
        let version: Option<String> = Some("".to_string());
        let version_file: Option<Utf8PathBuf> = None;
        let version = get_string_or_file_content(
            &version,
            &version_file,
            "unversioned",
            "version and version_file cannot both be supplied",
        )
        .unwrap();
        assert_eq!("unversioned".to_string(), version);
    }

    #[test]
    fn test_version_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(&mut file, "version_file").unwrap();

        let version: Option<String> = None;
        let version_file: Option<Utf8PathBuf> =
            Some(Utf8Path::from_path(file.path()).unwrap().into());
        let version = get_string_or_file_content(
            &version,
            &version_file,
            "unversioned",
            "version and version_file cannot both be supplied",
        )
        .unwrap();
        assert_eq!("version_file".to_string(), version);
    }

    #[test]
    fn test_empty_version_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(&mut file, "").unwrap();

        let version: Option<String> = None;
        let version_file: Option<Utf8PathBuf> =
            Some(Utf8Path::from_path(file.path()).unwrap().into());
        let version = get_string_or_file_content(
            &version,
            &version_file,
            "unversioned",
            "version and version_file cannot both be supplied",
        )
        .unwrap();
        assert_eq!("unversioned".to_string(), version);
    }

    #[test]
    fn test_error_for_both_versions() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(&mut file, "version_file").unwrap();

        let version: Option<String> = Some("version_string".to_string());
        let version_file: Option<Utf8PathBuf> =
            Some(Utf8Path::from_path(file.path()).unwrap().into());
        assert!(
            get_string_or_file_content(
                &version,
                &version_file,
                "unversioned",
                "version and version_file cannot both be supplied",
            )
            .is_err()
        );
    }

    #[test]
    fn test_version_file_missing() {
        let version: Option<String> = None;
        let version_file: Option<Utf8PathBuf> = Some(Utf8PathBuf::new());
        assert!(
            get_string_or_file_content(
                &version,
                &version_file,
                "unversioned",
                "version and version_file cannot both be supplied",
            )
            .is_err()
        );
    }

    #[test]
    fn test_validate_string() {
        let s = "0123579abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.-_".to_string();
        assert!(validate_string_for_upstream_versioning(s).is_ok());
    }

    #[test]
    fn test_validate_string_forward_slash() {
        let s = "a/b".to_string();
        assert!(validate_string_for_upstream_versioning(s).is_ok());
    }

    #[test]
    fn test_validate_string_too_long() {
        let s = "01234567890123456789012345678901234567890123456789012345678901234".to_string();
        assert_eq!(s.len(), 65);
        assert!(validate_string_for_upstream_versioning(s).is_err());
    }

    #[test]
    fn test_validate_string_invalid_first_char() {
        let s = ".a".to_string();
        assert!(validate_string_for_upstream_versioning(s).is_err());
    }

    #[test]
    fn test_validate_string_invalid_char() {
        let s = "a?b".to_string();
        assert!(validate_string_for_upstream_versioning(s).is_err());
    }
}
