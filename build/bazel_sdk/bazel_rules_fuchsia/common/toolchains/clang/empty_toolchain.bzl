# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Repository rule to define an empty host C++ toolchain."""

load(
    "@bazel_tools//tools/cpp:cc_toolchain_config_lib.bzl",
    "tool_path",
)
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")

def _empty_cc_toolchain_config_impl(ctx):
    # See CppConfiguration.java class in Bazel sources for the list of
    # all tool_path() names that must be defined and relative to the
    # clang repository directory.
    tool_paths = [
        tool_path(name = "ar", path = "/usr/bin/false"),
        tool_path(name = "cpp", path = "/usr/bin/false"),
        tool_path(name = "gcc", path = "/usr/bin/false"),
        tool_path(name = "gcov", path = "/usr/bin/false"),
        tool_path(name = "gcov-tool", path = "/usr/bin/false"),
        tool_path(name = "ld", path = "/usr/bin/false"),
        tool_path(name = "llvm-cov", path = "/usr/bin/false"),
        tool_path(name = "nm", path = "/usr/bin/false"),
        tool_path("objcopy", path = "/usr/bin/false"),
        tool_path("objdump", path = "/usr/bin/false"),
        tool_path("strip", path = "/usr/bin/false"),
        tool_path(name = "dwp", path = "/usr/bin/false"),
        tool_path(name = "llvm-profdata", path = "/usr/bin/false"),
    ]

    features = []

    return cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        toolchain_identifier = "empty_cpp",
        tool_paths = tool_paths,
        features = features,
        compiler = ctx.attr.compiler,
        # Required by constructor, but otherwise ignored by Bazel.
        # These string values are arbitrary, but are easy to grep
        # in our source tree if they ever happen to appear in
        # build error messages.
        host_system_name = "__bazel_host_system_name__",
        target_system_name = "__bazel_target_system_name__",
        target_libc = "__bazel_target_libc__",
        abi_version = "__bazel_abi_version__",
        abi_libc_version = "__bazel_abi_libc_version__",
    )

empty_cc_toolchain_config = rule(
    implementation = _empty_cc_toolchain_config_impl,
    doc = "Define a cc_toolchain_config target for an empty C++ toolchain.",
    attrs = {
        "compiler": attr.string(
            doc = "Compiler type, see https://github.com/bazelbuild/rules_cc/blob/main/cc/compiler/BUILD",
            default = "clang",
        ),
    },
)

def _empty_host_cpp_toolchain_repository_impl(repo_ctx):
    _BUILD_BAZEL_CONTENT = """
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load("@rules_cc//cc/toolchains:cc_toolchain.bzl", "cc_toolchain")
load("//common:toolchains/clang/empty_toolchain.bzl", "empty_cc_toolchain_config")

package(default_visibility = ["//visibility:public"])

# An empty filegroup.
filegroup(
  name = "empty",
  srcs = [],
)

empty_cc_toolchain_config(
  name = "empty_cc_toolchain_config",
)

cc_toolchain(
  name = "empty_cc_toolchain",
  all_files = ":empty",
  ar_files = ":empty",
  as_files = ":empty",
  compiler_files = ":empty",
  dwp_files = ":empty",
  linker_files = ":empty",
  objcopy_files = ":empty",
  strip_files = ":empty",
  supports_param_files = 0,
  toolchain_config = ":empty_cc_toolchain_config",
  toolchain_identifier = "empty_cc_toolchain",
)

toolchain(
  name = "empty_cpp_toolchain",
  exec_compatible_with = HOST_CONSTRAINTS,
  target_compatible_with = HOST_CONSTRAINTS,
  toolchain = ":empty_cc_toolchain",
  toolchain_type = "@bazel_tools//tools/cpp:toolchain_type",
)
"""
    repo_ctx.file("WORKSPACE.bazel", "")
    repo_ctx.symlink(repo_ctx.path(Label("//common:BUILD.bazel")).dirname, "common")
    repo_ctx.file("BUILD.bazel", _BUILD_BAZEL_CONTENT)

empty_host_cpp_toolchain_repository = repository_rule(
    implementation = _empty_host_cpp_toolchain_repository_impl,
    doc = """Generate a repository that contains an empty C++ toolchain definition.

Useful when running on machines without an installed GCC or Clang.
Usage example, from a MODULE.bazel file:

empty_host_cpp_toolchain_repository = use_repo_rule(
    "@rules_fuchsia//common:toolchains/clang/empty_toolchain.bzl",
    "empty_host_cpp_toolchain_repository",
)

empty_host_cpp_toolchain_repository(
    name = "host_no_cpp",
)

register_toolchains("@host_no_cpp//:empty_cpp_toolchain")
""",
)
