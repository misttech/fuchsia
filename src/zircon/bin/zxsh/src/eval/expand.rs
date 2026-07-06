// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::execution_context::ExecutionContext;
use super::state::ShellState;
use bstr::{BStr, BString};

/// Helper to parse and expand parameter modifier words.
/// Note: this function will become more elaborate in a later CL.
pub(crate) fn parse_and_expand_modifier(
    modifier_str: &BStr,
    _state: &mut ShellState,
    _ctx: &ExecutionContext,
) -> Result<BString, String> {
    Ok(BString::from(modifier_str))
}
