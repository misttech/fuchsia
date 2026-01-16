# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

def with_fuchsia_rustc_flags(rustc_flags):
    """Add a list of Fuchsia-specific rustc flags to input rustc_flags."""

    if rustc_flags == None:
        rustc_flags = []

    return rustc_flags + [
        # --cap-lints can't be overridden once set, see https://rust-lang.github.io/rfcs/1193-cap-lints.html.
        #
        # As a result, we avoid setting this on the toolchain directly, which will affect
        # third-party rust-crates.
        #
        # This config matches `rust_cap_lints_default` in GN defined in //build/rust/config.gni.
        "--cap-lints=deny",
    ]
