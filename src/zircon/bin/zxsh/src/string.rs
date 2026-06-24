// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Parse an integer from a byte slice, ignoring leading and trailing ASCII whitespace.
/// Supports optional leading '+' or '-'.
pub fn parse_int<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
    let trimmed = bytes.trim_ascii();
    if trimmed.is_empty() {
        return None;
    }
    std::str::from_utf8(trimmed).ok()?.parse::<T>().ok()
}
