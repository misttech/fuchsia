# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A macro for defining a Rust dylib with optional unit tests."""

load(
    "@fuchsia_rules_common//build_flags:rust.bzl",
    "BUILD_FLAGS_RUST_MACRO_ATTRS",
    "wrap_rust_macro_args_with_build_flags",
)
load("@rules_rust//rust:defs.bzl", "rust_dylib_library")
load(
    "//build/bazel/rules/rust:common.bzl",
    "with_fuchsia_rust_link_exec_properties",
    "with_fuchsia_rust_stdlib_link_deps",
    "with_fuchsia_rust_tags",
    "with_fuchsia_rustc_flags",
)
load("//build/bazel/rules/rust:generate_unit_tests.bzl", "generate_unit_tests")

def _rustc_dylib_impl(
        name,
        with_unit_tests,
        test_deps,
        lint_config,
        disable_clippy,
        rustc_flags,
        build_flags,
        output_name,  # buildifier: disable=unused-variable
        public_deps,
        visibility,
        **kwargs):
    # `output_name` is only consumed by the GN rustc_dylib() template generated
    # by bazel2gn. rules_rust always names dylibs `lib<crate_name>-<hash>.so`.

    # rules_rust propagates link dependencies transitively, so `public_deps`
    # are regular native link dependencies in Bazel. They only differ in GN.
    if public_deps:
        kwargs["link_deps"] = (kwargs.get("link_deps") or []) + public_deps
    if lint_config == None:
        lint_config = "//build/config/rust/lints:clippy_warn_production"
        test_lint_config = "//build/config/rust/lints:clippy_warn_default"
    else:
        test_lint_config = lint_config

    kwargs["tags"] = with_fuchsia_rust_tags(kwargs.get("tags"), disable_clippy)
    kwargs["rustc_flags"] = with_fuchsia_rustc_flags(rustc_flags)

    library_kwargs = wrap_rust_macro_args_with_build_flags(
        kwargs = kwargs,
        name = name,
        rust_rule_name = "rust_dylib_library",
        build_flags = build_flags,
        target_type = "rust_shared_library",
    )

    rust_dylib_library(
        name = name,
        lint_config = lint_config,
        # The Fuchsia Rust toolchain already passes -Cprefer-dynamic to every
        # crate, so libstd is dynamically linked. Setting this to True would
        # also add libstd-<hash>.so to the dylib's runfiles, which causes it
        # to be installed twice by fx_packaged_binary().
        link_std_dylib = False,
        visibility = visibility,
        **(library_kwargs | {
            "exec_properties": with_fuchsia_rust_link_exec_properties(library_kwargs.get("exec_properties")),
            "link_deps": with_fuchsia_rust_stdlib_link_deps(library_kwargs.get("link_deps")),
        })
    )

    if with_unit_tests:
        test_kwargs = wrap_rust_macro_args_with_build_flags(
            kwargs = kwargs,
            name = "{}_test".format(name),
            rust_rule_name = "rust_test",
            build_flags = build_flags,
            target_type = "rust_executable",
        )

        # Not supported by rust_test(), see rustc_library.bzl.
        test_kwargs.pop("disable_pipelining", None)

        generate_unit_tests(
            name = name,
            with_host_unit_tests = False,
            with_unit_tests = with_unit_tests,
            test_deps = test_deps,
            lint_config = test_lint_config,
            visibility = visibility,
            **test_kwargs
        )

rustc_dylib = macro(
    doc = """`rust_dylib_library` wrapper with Fuchsia-specific features.

Builds a Rust `dylib` crate, i.e. a shared library using the unstable Rust ABI
that other Rust crates can depend on. Rust dylibs are only supported when
targeting Fuchsia, so targets using this macro should set
`target_compatible_with = ["@platforms//os:fuchsia"]`.

Apply Fuchsia-specific Rust flags.

Generate a test target when with_unit_tests is enabled. The test target will
be named "<name>_test" and will include extra dependencies from test_deps.

The default lint_config value is //build/config/rust/lints:clippy_warn_production
for the target, and //build/config/rust/lints:clippy_warn_default for the test target.
If specified by the caller, lint_config is applied to both main and test targets.

Also used to allow easier syncing between Bazel and GN targets, where this
maps to the GN rustc_dylib() template.
""",
    implementation = _rustc_dylib_impl,
    inherit_attrs = rust_dylib_library,
    attrs = {
        # Always False, see comment in _rustc_dylib_impl().
        "link_std_dylib": None,
        "with_unit_tests": attr.bool(
            doc = "If true, a `rust_test` target will be created.",
            default = False,
            configurable = False,
        ),
        "test_deps": attr.label_list(
            doc = "Extra dependencies for the test target.",
            default = [],
        ),
        "disable_clippy": attr.bool(
            doc = "If true, disables clippy lints on this target.",
            default = False,
            configurable = False,
        ),
        "public_deps": attr.label_list(
            doc = """Native (e.g. C/C++) dependencies that are also exposed to dependents in GN.

In Bazel, these are equivalent to `link_deps`, since rules_rust propagates link
dependencies transitively. In the GN rustc_dylib() template generated by
bazel2gn, these are forwarded as GN `public_deps`, which is required for
non-Rust shared libraries that dependents of the dylib also need to link
against (see https://fxbug.dev/42137337).""",
            default = [],
        ),
        "output_name": attr.string(
            doc = """Base name of the shared library in GN, i.e. lib<output_name>.so.

Only used by the GN rustc_dylib() template generated by bazel2gn. rules_rust
does not support renaming dylibs, which are always named
`lib<crate_name>-<hash>.so` in Bazel.""",
            configurable = False,
        ),
    } | BUILD_FLAGS_RUST_MACRO_ATTRS,
)
