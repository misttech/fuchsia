# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro for defining a Fuchsia C++ shared library."""

load(
    "@fuchsia_rules_common//build_flags:cc.bzl",
    "BUILD_FLAGS_CC_MACRO_ATTRS",
    "wrap_cc_macro_args_with_build_flags",
)
load("@rules_cc//cc:defs.bzl", "cc_shared_library")
load(
    "//build/bazel/rules/cc:instrumentation.bzl",
    "CC_INSTRUMENTATION_ATTRS",
    "get_cc_instrumentation_features",
)

visibility(["//..."])

def get_shared_library_name_and_soname(name, shared_lib_name = None):
    """Compute the output shared library path and DT_SONAME for a target.

    When `shared_lib_name` is not provided, this matches the output filename
    produced by `@rules_cc` for `cc_binary(linkshared = True)` (and applies
    the same naming to `fx_cc_shared_library()` by setting `shared_lib_name`):
    - If the target basename ends with ".so" or contains ".so." (e.g.
      "libfoo.so", "foo.so", or "libfoo.so.1"), the output filename is
      `name` as-is.
    - Otherwise (e.g. "foo"), the toolchain's DYNAMIC_LIBRARY pattern
      ("lib<basename>.so") is used, producing "libfoo.so".

    Args:
        name: The target name (may include a relative subdirectory prefix).
        shared_lib_name: Optional explicit shared library output path.

    Returns:
        A tuple of `(shared_lib_name, soname)`.
    """
    if not shared_lib_name:
        base_name = name.split("/")[-1]
        prefix = name[:-len(base_name)]
        is_so_name = base_name.endswith(".so") or ".so." in base_name
        file_name = base_name if is_so_name else "lib{}.so".format(base_name)
        shared_lib_name = prefix + file_name
    soname = shared_lib_name.split("/")[-1]
    return shared_lib_name, soname

def _fx_cc_shared_library_impl(
        name,
        public_configs,  # buildifier: disable=unused-variable - For GN conversion only.
        build_flags,
        disable_build_flags,
        disable_instrumentation,
        features,
        **kwargs):
    """Implementation for the fx_cc_shared_library() macro."""

    features = get_cc_instrumentation_features(features, disable_instrumentation)

    shared_lib_name, soname = get_shared_library_name_and_soname(
        name,
        kwargs.get("shared_lib_name"),
    )
    kwargs["shared_lib_name"] = shared_lib_name
    kwargs["features"] = features

    wrapped_kwargs = wrap_cc_macro_args_with_build_flags(
        kwargs = kwargs,
        name = name,
        cc_rule_name = "cc_shared_library",
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        target_type = "cxx_shared_library",
    )

    # Prepend default -Wl,-soname=<soname> before the build_flags response file
    # and user_link_flags so that custom soname flags in build_flags(ldflags = ...)
    # or user_link_flags appear later on the linker command line and override it.
    wrapped_kwargs["user_link_flags"] = [
        "-Wl,-soname={}".format(soname),
    ] + (wrapped_kwargs.get("user_link_flags") or [])

    cc_shared_library(
        name = name,
        **wrapped_kwargs
    )

fx_cc_shared_library = macro(
    doc = """Wrapper for cc_shared_library() shared libraries for Fuchsia.

    Toolchain overrides can be specified using build_flags and
    disable_build_flags. These will not affect dependencies.
    """,
    implementation = _fx_cc_shared_library_impl,
    # TODO(https://fxbug.dev/446694542): Remove `native.` once the
    # `cc_shared_library()` wrapper is a symbolic macro.
    inherit_attrs = native.cc_shared_library,
    attrs = {
        "public_configs": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
    } | CC_INSTRUMENTATION_ATTRS | BUILD_FLAGS_CC_MACRO_ATTRS,
)
