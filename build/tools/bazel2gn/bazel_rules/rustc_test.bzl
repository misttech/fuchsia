# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@rules_rust//rust:defs.bzl", "rust_test")

# rustc_test defines a Rust test target with Fuchsia-specific lint config by default.
def rustc_test(name, **kwargs):
    kwargs["lint_config"] = kwargs.get(
        "lint_config",
        "//build/config/rust/lints:clippy_warn_default",
    )
    rust_test(
        name = name,
        **kwargs
    )
