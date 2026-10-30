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
load(
    "//build/bazel/rules/cc:fx_cc_shared_library.bzl",
    "get_shared_library_name_and_soname",
)
load(
    "//build/bazel/rules/cc:instrumentation.bzl",
    "CC_INSTRUMENTATION_ATTRS",
    "get_cc_instrumentation_features",
)

def _fx_cc_binary_impl(
        name,
        linkshared,
        public_configs,  # buildifier: disable=unused-variable - For GN conversion only.
        gn_output_name,  # buildifier: disable=unused-variable - For GN conversion only.
        disable_syslog_backend,  # buildifier: disable=unused-variable - For GN conversion only.
        build_flags,
        disable_build_flags,
        disable_instrumentation,
        features,
        **kwargs):
    """Implementation for the fx_cc_binary() macro."""

    features = get_cc_instrumentation_features(features, disable_instrumentation)

    wrapped_kwargs = wrap_cc_macro_args_with_build_flags(
        kwargs = kwargs | {"features": features, "linkshared": linkshared},
        name = name,
        cc_rule_name = "cc_binary",
        # LINT.IfChange(fdio_config)
        # Link libfdio.so into Fuchsia executables by default. This is a no-op
        # for host binaries (fdio_config has no deps there) and for shared
        # libraries (executable_link_deps are ignored for them). Targets can
        # opt out with disable_build_flags.
        build_flags = [Label("//build/config/fuchsia:fdio_config")] + build_flags,
        # LINT.ThenChange(//build/bazel/config/BUILD.gn:fdio_config)
        disable_build_flags = disable_build_flags,

        # cc_binary() produces a shared library if linkshared=True, or
        # an executable if it is False (the default).
        target_type = "cxx_shared_library" if linkshared else "cxx_executable",
    )

    if linkshared:
        # Prepend default -Wl,-soname=<soname> before the build_flags response
        # file and linkopts so that custom soname flags in build_flags(ldflags = ...)
        # or linkopts appear later on the linker command line and override it.
        _, soname = get_shared_library_name_and_soname(name)
        wrapped_kwargs["linkopts"] = [
            "-Wl,-soname={}".format(soname),
        ] + (wrapped_kwargs.get("linkopts") or [])

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
        # Override the configurable `linkshared` inherited from cc_binary():
        # symbolic macros see configurable values as select() objects (which
        # are always truthy), but target_type must be known at loading time.
        "linkshared": attr.bool(
            doc = "Produce a shared library instead of an executable.",
            default = False,
            configurable = False,
        ),
        "public_configs": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
        "gn_output_name": attr.string(
            doc = """Unused in Bazel, for GN conversion only.

            Sets `output_name` on the GN target that bazel2gn generates, e.g.
            to avoid duplicate output files in GN's flat `$root_out_dir`. The
            Bazel binary is always named after the target; use
            `fx_packaged_binary(binary_name = ...)` to choose its name in a
            Fuchsia package.
            """,
            default = "",
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
    } | CC_INSTRUMENTATION_ATTRS | BUILD_FLAGS_CC_MACRO_ATTRS,
)
