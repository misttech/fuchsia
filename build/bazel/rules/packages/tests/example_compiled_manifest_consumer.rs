// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Embeds component manifests compiled by `fx_compiled_manifest()`, the way
//! component_manager embeds its builtin component manifests.

pub static EXAMPLE_CM: &[u8] = include_bytes!(env!("EXAMPLE_CM_PATH"));
pub static CUSTOM_OUTPUT_NAME_CM: &[u8] = include_bytes!(env!("CUSTOM_OUTPUT_NAME_CM_PATH"));
