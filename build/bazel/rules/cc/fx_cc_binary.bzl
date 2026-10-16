# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro for defining a Fuchsia C++ binary."""

load(
    "@fuchsia_rules_common//build_flags:cc.bzl",
    "BUILD_FLAGS_CC_MACRO_ATTRS",
    "wrap_cc_macro_args_with_build_flags",
)
load("@rules_cc//cc:defs.bzl", "cc_binary")

# The C++ toolchain features that build a target with instrumentation, such as
# sanitizers. These are disabled for targets that set
# `disable_instrumentation = True`.
#
# LINT.IfChange(instrumentation_features)
_INSTRUMENTATION_FEATURES = [
    "asan",
    "coverage",
    "hwasan",
    "lsan",
    "msan",
    "tsan",
    "ubsan",
]
# LINT.ThenChange(//build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/sanitizer.bzl:sanitizer_features)

def _fx_cc_binary_impl(
        name,
        public_configs,  # buildifier: disable=unused-variable - For GN conversion only.
        disable_syslog_backend,  # buildifier: disable=unused-variable - For GN conversion only.
        build_flags,
        disable_build_flags,
        disable_instrumentation,
        features,
        **kwargs):
    """Implementation for the fx_cc_binary() macro."""

    if disable_instrumentation:
        features = (features or []) + ["-" + f for f in _INSTRUMENTATION_FEATURES]

    wrapped_kwargs = wrap_cc_macro_args_with_build_flags(
        kwargs = kwargs | {"features": features},
        name = name,
        cc_rule_name = "cc_binary",
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,

        # cc_binary() produces a shared library if linkshared=True, or
        # an executable if it is False (the default).
        target_type = "cxx_shared_library" if kwargs.get("linkshared") else "cxx_executable",
    )

    cc_binary(
        name = name,
        **wrapped_kwargs
    )

fx_cc_binary = macro(
    doc = """Wrapper for cc_binary() binaries for Fuchsia.

    Toolchain overrides can be specified using build_flags and
    disable_build_flags. These will not affect dependencies.
    """,
    implementation = _fx_cc_binary_impl,
    # TODO(https://fxbug.dev/446694542): Remove `native.` once the
    # `cc_binary()` wrapper is a symbolic macro.
    inherit_attrs = native.cc_binary,
    attrs = {
        "public_configs": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
        "disable_instrumentation": attr.bool(
            doc = """Never build this binary with instrumentation enabled (e.g. sanitizers).

            This disables the toolchain features listed in
            `_INSTRUMENTATION_FEATURES` for this target. Unlike GN's
            `exclude_toolchain_tags = [ "instrumented" ]`, which bazel2gn
            generates from this attribute, this does not affect the
            target's dependencies. These are still built with
            instrumentation and will likely need the instrumentation
            runtime, so this currently only works for binaries without
            dependencies.

            TODO(b/568698143): Apply the features to dependencies too with
            a transition, like `transitive_features` in the SDK
            `fuchsia_cc_binary()`.
            """,
            default = False,
            configurable = False,
        ),
        "disable_syslog_backend": attr.bool(
            doc = """Unused in Bazel, for GN conversion only.

            In GN, this asserts that the executable does not depend on the
            syslog backend. Bazel binaries do not get an implicit syslog
            backend dependency.
            """,
            default = False,
            configurable = False,
        ),
    } | BUILD_FLAGS_CC_MACRO_ATTRS,
)
