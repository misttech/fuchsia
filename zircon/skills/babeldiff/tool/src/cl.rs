// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Support for downloading changes directly from Gerrit (fuchsia-review.googlesource.com).

use std::process::Command;

/// Parses a review specifier into a change number and optional patchset number.
///
/// Accepts:
/// - Plain change numbers: `"1885120"`
/// - Change and patchset numbers: `"1885120/5"`
/// - `fxr/` prefixed identifiers: `"fxr/1885120"`, `"fxr/1885120/5"`
/// - Gerrit web URLs: `"https://fuchsia-review.googlesource.com/c/fuchsia/+/1885120"`,
///   `"https://fuchsia-review.googlesource.com/c/fuchsia/+/1885120/5"`,
///   `"https://fuchsia-review.git.corp.google.com/c/fuchsia/+/1885120/5"`,
///   `"https://fxr.dev/1885120"`, etc.
pub fn parse_cl_spec(input: &str) -> Result<(String, Option<String>), String> {
    let clean = input.trim();
    if clean.is_empty() {
        return Err("review specifier cannot be empty".to_string());
    }
    // Strip query parameters and fragments if present.
    let clean = clean.split('?').next().unwrap_or(clean);
    let clean = clean.split('#').next().unwrap_or(clean);

    let remainder = if let Some((_, after)) = clean.split_once("/+/") {
        after
    } else if let Some(after) =
        clean.strip_prefix("http://").or_else(|| clean.strip_prefix("https://"))
    {
        // Strip host
        match after.split_once('/') {
            Some((_, path)) => path,
            None => "",
        }
    } else {
        clean
    };

    let remainder = remainder
        .strip_prefix("fxr/")
        .or_else(|| remainder.strip_prefix("fxrev.dev/"))
        .unwrap_or(remainder);

    let parts: Vec<&str> = remainder.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Err(format!("invalid review specifier: {input:?}"));
    }

    let change_num = parts[0];
    if change_num.is_empty() || !change_num.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "invalid change number {change_num:?} in {input:?}: must consist of digits"
        ));
    }

    let patchset = if parts.len() > 1 {
        let ps = parts[1];
        if ps.is_empty() || !ps.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("invalid patchset {ps:?} in {input:?}: must consist of digits"));
        }
        Some(ps.to_string())
    } else {
        None
    };

    Ok((change_num.to_string(), patchset))
}

/// Constructs the Gerrit patch download URL with full context (`context=100000`).
pub fn gerrit_patch_url(change_num: &str, patchset: Option<&str>) -> String {
    let base_url = std::env::var("BABELDIFF_GERRIT_URL")
        .unwrap_or_else(|_| "https://fuchsia-review.googlesource.com".to_string());
    let base_url = base_url.trim_end_matches('/');
    let rev = patchset.unwrap_or("current");
    format!("{base_url}/changes/{change_num}/revisions/{rev}/patch?raw&context=100000")
}

/// Downloads patch text from Gerrit using `curl`.
pub fn download_patch(url: &str) -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .map_err(|e| format!("failed to run curl to download patch: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.trim();
        if msg.is_empty() {
            return Err(format!(
                "failed to download patch from {url} (curl exited with {})",
                out.status
            ));
        } else {
            return Err(format!(
                "failed to download patch from {url} (curl exited with {}): {msg}",
                out.status
            ));
        }
    }
    String::from_utf8(out.stdout).map_err(|e| format!("patch from {url} is not valid UTF-8: {e}"))
}

/// Extracts the commit SHA from the `From <sha> ` header of a git format-patch text.
pub fn patch_commit_sha(text: &str) -> Option<String> {
    for line in text.lines().take(5) {
        if let Some(rest) = line.strip_prefix("From ") {
            if let Some(sha) = rest.split_whitespace().next() {
                if sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(sha.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cl_formats() {
        assert_eq!(parse_cl_spec("1885120"), Ok(("1885120".into(), None)));
        assert_eq!(parse_cl_spec("  1885120  "), Ok(("1885120".into(), None)));
        assert_eq!(parse_cl_spec("1885120/5"), Ok(("1885120".into(), Some("5".into()))));
        assert_eq!(parse_cl_spec("fxr/1885120"), Ok(("1885120".into(), None)));
        assert_eq!(parse_cl_spec("fxr/1885120/5"), Ok(("1885120".into(), Some("5".into()))));
        assert_eq!(
            parse_cl_spec("https://fuchsia-review.googlesource.com/c/fuchsia/+/1885120"),
            Ok(("1885120".into(), None))
        );
        assert_eq!(
            parse_cl_spec("https://fuchsia-review.googlesource.com/c/fuchsia/+/1885120/5"),
            Ok(("1885120".into(), Some("5".into())))
        );
        assert_eq!(
            parse_cl_spec("https://fuchsia-review.git.corp.google.com/c/fuchsia/+/1885120/5"),
            Ok(("1885120".into(), Some("5".into())))
        );
        assert_eq!(
            parse_cl_spec("https://fuchsia-review.googlesource.com/c/fuchsia/+/1885120/5/"),
            Ok(("1885120".into(), Some("5".into())))
        );
        assert_eq!(
            parse_cl_spec(
                "https://fuchsia-review.googlesource.com/c/fuchsia/+/1885120/5?tab=comments"
            ),
            Ok(("1885120".into(), Some("5".into())))
        );
        assert_eq!(parse_cl_spec("https://fxr.dev/1885120"), Ok(("1885120".into(), None)));
        assert_eq!(
            parse_cl_spec("https://fxr.dev/1885120/3"),
            Ok(("1885120".into(), Some("3".into())))
        );
    }

    #[test]
    fn rejects_invalid_formats() {
        assert!(parse_cl_spec("").is_err());
        assert!(parse_cl_spec("   ").is_err());
        assert!(parse_cl_spec("invalid").is_err());
        assert!(parse_cl_spec("1885120/abc").is_err());
        assert!(parse_cl_spec("fxr/").is_err());
        assert!(parse_cl_spec("https://fuchsia-review.googlesource.com").is_err());
    }

    #[test]
    fn extracts_patch_commit_sha() {
        let text = "From 5cf9e488f340ac628045c30a3b9db699552f856a Mon Sep 17 00:00:00 2001\nSubject: Test\n";
        assert_eq!(patch_commit_sha(text), Some("5cf9e488f340ac628045c30a3b9db699552f856a".into()));
        assert_eq!(patch_commit_sha("Subject: Test\n"), None);
    }
}
