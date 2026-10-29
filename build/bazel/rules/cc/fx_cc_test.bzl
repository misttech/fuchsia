# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro for defining a Fuchsia C++ test executable."""

load("//build/bazel/rules/cc:fx_cc_binary.bzl", "fx_cc_binary")

visibility(["//..."])

def _fx_cc_test_impl(
        name,
        visibility,
        testonly,
        disable_lto,
        build_flags,
        **kwargs):
    """Implementation for the fx_cc_test() macro."""

    if testonly != None and not testonly:
        fail("`fx_cc_test()` targets are always testonly.")

    # Link test executables with `--no-fat-lto-objects` by default, matching
    # GN's `cc_test_executable()` (used by `//build/test.gni`). This is appended
    # after the caller's `build_flags`, like GN's `configs += [...]`, so that it
    # takes precedence in `ld.lld`. Targets can opt out with
    # `disable_lto = False`.
    #
    # TODO(https://fxbug.dev/571444250): This does not opt out of LTO, it only
    # links the native code of fat LTO objects. Use a proper LTO opt-out once
    # it is available.
    if disable_lto:
        build_flags = (build_flags or []) + [
            Label("//build/config/lto:no-fat-lto-objects-linking"),
        ]

    fx_cc_binary(
        name = name,
        visibility = visibility,
        testonly = True,
        build_flags = build_flags,
        **kwargs
    )

fx_cc_test = macro(
    doc = """Defines a C/C++ test executable for Fuchsia and host builds.

    Targets defined with `fx_cc_test()` are always `testonly = True`, and are
    linked with `//build/config/lto:no-fat-lto-objects-linking` by default
    (`disable_lto = True`).

    This is the Bazel counterpart of GN's `test()` template (from
    `//build/test.gni`) and syncs to `test()` via `bazel2gn`. Like
    `fx_cc_binary()`, the resulting target is an executable binary rather than a
    standalone Bazel test rule: wrap it with `fx_unittest_package()`,
    `fx_unittest_component()`, or `fx_packaged_binary()` + `fx_test_component()`
    for Fuchsia device tests, or `host_test()` for host tests.
    """,
    implementation = _fx_cc_test_impl,
    inherit_attrs = fx_cc_binary,
    attrs = {
        "disable_lto": attr.bool(
            doc = """Links with `//build/config/lto:no-fat-lto-objects-linking`.

            Mirrors `disable_lto` on GN's `cc_test_executable()`, which adds the
            same config. Set to `False` when the test specifically exercises
            LTO linking behavior.
            """,
            default = True,
            configurable = False,
        ),
        # GN `test()` always produces an executable, never a shared library.
        "linkshared": None,
    },
)
