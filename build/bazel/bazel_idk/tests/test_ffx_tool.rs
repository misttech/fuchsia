// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// ffx embeds and extracts version information in `.ffx_version` and `.ffx_build`
// link sections (injected into the versioned binary by `ffx_apply_version`).
#[used]
#[unsafe(no_mangle)]
#[cfg_attr(target_os = "macos", unsafe(link_section = ".FFX_VERSION,.ffx_version"))]
#[cfg_attr(not(target_os = "macos"), unsafe(link_section = ".ffx_version"))]
static VERSION_INFO: [u8; 64] = [b'v'; 64];

#[used]
#[unsafe(no_mangle)]
#[cfg_attr(target_os = "macos", unsafe(link_section = ".FFX_BUILD,.ffx_build"))]
#[cfg_attr(not(target_os = "macos"), unsafe(link_section = ".ffx_build"))]
static BUILD_VERSION: [u8; 64] = [b'v'; 64];

fn main() {
    let out = std::env::args().last().expect("missing metadata output path");
    std::fs::write(out, "{}\n").expect("failed to write stub metadata");
}
