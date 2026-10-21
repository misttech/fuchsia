# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared default compiler flag constants for both in-tree build_flags() and OOT cc_features.bzl."""

# LINT.IfChange(default_warnings)
DEFAULT_WARNINGS_DEFINES = [
    # TODO(https://fxbug.dev/542707525): Suppress unused variable warnings in libc++ containers.
    "_LIBCPP_DISABLE_UNUSED_STRUCT_WARNINGS",
]

DEFAULT_WARNINGS_CFLAGS = [
    "-Wall",
    "-Wextra",
    "-Wextra-semi",
    "-Wnewline-eof",
    # TODO(b/315062126) Some in-tree builds are failing because we
    # are shadowing variables.
    #"-Wshadow",
    "-Wstrict-prototypes",
    "-Wwrite-strings",
    "-Wno-sign-conversion",
    "-Wno-unused-parameter",
    "-Wnonportable-system-include-path",

    # This is still needed for -Wmaybe-uninitialized which is added as a pragma in source code.
    # Clang hasn't implemented this yet.
    "-Wno-unknown-warning-option",
    "-Wno-missing-field-initializers",

    # TODO(https://fxbug.dev/477786942): Fix violations and enable the following checks.
    # "-Wconversion",
    # "-Wimplicit-fallthrough",

    # TODO(https://fxbug.dev/534361287): Clean up unused templates in first-party code.
    # This is currently applied globally to unblock the toolchain update.
    # Eventually, this should be removed from default_warnings and only applied
    # to third_party targets.
    "-Wno-unused-template",
]
# LINT.ThenChange(//build/config/BUILD.gn:default_warnings)

# LINT.IfChange(thread_safety_annotations)
THREAD_SAFETY_DEFINES = [
    "_LIBCPP_ENABLE_THREAD_SAFETY_ANNOTATIONS=1",
]

THREAD_SAFETY_CFLAGS = [
    "-Wthread-safety",

    # TODO(https://fxbug.dev/42085252): Clang is catching instances of these in the kernel and drivers.
    # Temporarily disable them for now to facilitate the roll then come back and
    # fix them.
    "-Wno-unknown-warning-option",
    "-Wno-thread-safety-reference-return",
]
# LINT.ThenChange(
#     //build/config/BUILD.bazel:thread_safety_annotations,
#     //build/config/BUILD.gn:thread_safety_annotations,
# )
