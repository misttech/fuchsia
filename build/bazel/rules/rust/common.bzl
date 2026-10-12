# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@fuchsia_build_info//:args.bzl", "rust_cap_lints")

def with_fuchsia_rustc_flags(rustc_flags):
    """Add a list of Fuchsia-specific rustc flags to input rustc_flags."""
    return (rustc_flags or []) + [
        # --cap-lints can't be overridden once set, see https://rust-lang.github.io/rfcs/1193-cap-lints.html.
        #
        # As a result, we avoid setting this on the toolchain directly, which will affect
        # third-party rust-crates.
        "--cap-lints={}".format(rust_cap_lints),
    ]

def with_fuchsia_rust_stdlib_link_deps(link_deps):
    """Add native libraries needed by the Rust standard library to link_deps.

    Must only be used for targets that are linked, i.e. executables, tests and
    dylibs, see //build/bazel/rules/rust:fdio_for_rust_stdlib.
    """
    return (link_deps or []) + select({
        "@platforms//os:fuchsia": ["//build/bazel/rules/rust:fdio_for_rust_stdlib"],
        "//conditions:default": [],
    })

def with_fuchsia_rust_test_aspect_hints(aspect_hints):
    """Add the Fuchsia Rust test runner shard to aspect_hints."""
    return (aspect_hints or []) + ["//src/sys/test_runners/rust:default"]
