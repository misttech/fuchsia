// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bstr::BString;
use std::ffi::{CStr, CString, NulError};

/// Parse an integer from a byte slice, ignoring leading and trailing ASCII whitespace.
/// Supports optional leading '+' or '-'.
pub fn parse_int<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
    let trimmed = bytes.trim_ascii();
    if trimmed.is_empty() {
        return None;
    }
    std::str::from_utf8(trimmed).ok()?.parse::<T>().ok()
}

/// Converts a byte slice (e.g., `&BStr` or `&BString`) into a `CString` for FFI calls.
pub fn bstr_to_cstring(s: &[u8]) -> Result<CString, NulError> {
    CString::new(s)
}

/// Converts a slice of `BString`s into a vector of `CString`s for FFI calls.
pub fn bstrings_to_cstrings(strings: &[BString]) -> Result<Vec<CString>, NulError> {
    strings.iter().map(|s| bstr_to_cstring(s.as_slice())).collect()
}

/// Converts a slice of `CString`s into a vector of `&CStr` references for FFI calls.
pub fn cstrings_to_c_strs(cstrings: &[CString]) -> Vec<&CStr> {
    cstrings.iter().map(|s| s.as_c_str()).collect()
}
