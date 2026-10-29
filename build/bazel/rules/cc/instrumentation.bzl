# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Common instrumentation definitions for Fuchsia C++ macros."""

visibility("private")

# The C++ toolchain features that build a target with instrumentation, such as
# sanitizers. These are disabled for targets that set
# `disable_instrumentation = True`.
#
# LINT.IfChange(instrumentation_features)
_INSTRUMENTATION_FEATURES = [
    "asan",
    "coverage",
    "coverage-rust",
    "hwasan",
    "lsan",
    "msan",
    "profile",
    "profile-rust",
    "sancov",
    "tsan",
    "ubsan",
]
# LINT.ThenChange(//build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/sanitizer.bzl:sanitizer_features)

CC_INSTRUMENTATION_ATTRS = {
    "disable_instrumentation": attr.bool(
        doc = """Never build this target with instrumentation enabled (e.g. sanitizers).

        This disables the toolchain features listed in
        `_INSTRUMENTATION_FEATURES` for this target. Unlike GN's
        `exclude_toolchain_tags = [ "instrumented" ]`, which bazel2gn
        generates from this attribute, this does not affect the
        target's dependencies. These are still built with
        instrumentation and will likely need the instrumentation
        runtime, so this currently only works for targets without
        dependencies.

        TODO(b/568698143): Apply the features to dependencies too with
        a transition, like `transitive_features` in the SDK
        `fuchsia_cc_binary()`. Note that until a transition is used,
        this attribute also cannot be used with `fx_packaged_binary`,
        because packaging rules select the variant library directory
        from the build configuration.
        """,
        default = False,
        configurable = False,
    ),
}

def get_cc_instrumentation_features(features, disable_instrumentation):
    """Returns the features list adjusted for `disable_instrumentation`.

    Args:
        features: The input list of features (or None).
        disable_instrumentation: Boolean indicating whether instrumentation
            features should be disabled.

    Returns:
        The updated list of features.
    """
    if disable_instrumentation:
        return (features or []) + ["-" + f for f in _INSTRUMENTATION_FEATURES]
    return features
