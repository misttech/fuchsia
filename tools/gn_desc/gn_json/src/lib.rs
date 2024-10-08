// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Types used for parsing `gn desc --format=json` output.

use anyhow::Context;
use std::path::Path;

pub mod label;
pub mod target;

/// Parse a string containing GN's desc output.
///
/// This is a non-generic wrapper around `serde_json::from_str<>`, so that the
/// monomorphisation happens in this rlib, which is always compiled with
/// optimizations enabled.
///
pub fn from_str(s: &str) -> serde_json::Result<crate::target::AllTargets> {
    serde_json::from_str(s)
}

/// Parse a file containing GN's desc output.
///
/// This is an optimized implementation that trades memory (potentially a lot)
/// for speed when parsing the JSON produced by GN.
pub fn parse_file(path: impl AsRef<Path>) -> anyhow::Result<crate::target::AllTargets> {
    // Reading to a string before parsing is considerably faster than letting
    // serde parse from a BufferedReader (saves about 30% based on testing with
    // with large gn_desc.json files (>500MB).
    let path = path.as_ref();
    let gn_desc_json = std::fs::read_to_string(path)
        .with_context(|| format!("Unable to open file: {}", path.display()))?;
    serde_json::from_str(&gn_desc_json)
        .with_context(|| format!("Unable to parse file as json: {}", path.display()))
}
