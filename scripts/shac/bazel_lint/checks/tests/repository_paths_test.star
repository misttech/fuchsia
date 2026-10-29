# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_repository_paths."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_repository_paths",
)
load("./base_test.star", "run_lint_test")

_DOCS_SUFFIX = "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-use-fuchsia-bazel-sdk-paths."

_VIOLATION_TARGET = 'filegroup(name = "fuchsia_sdk_dep", deps = ["@fuchsia_sdk//pkg/zx"])\n'

def _run_repository_paths_test(files):
    return run_lint_test(_bazel_repository_paths, "bazel_repository_paths", files)

def _msg(label):
    return (
        "Do not use Fuchsia Bazel SDK path `%s` in platform code; use platform rules under `//build/bazel/rules/` or in-tree platform targets instead. %s" % (
            label,
            _DOCS_SUFFIX,
        )
    )

def test_disallowed_sdk_repository_paths():
    res = _run_repository_paths_test({
        "src/BUILD.bazel": (
            'load("@fuchsia_sdk//fuchsia:defs.bzl", "fuchsia_cc_binary")\n' +
            'load("@@fuchsia_sdk//fuchsia:defs.bzl", "fuchsia_cc_test")\n' +
            'load("@internal_sdk//fuchsia:defs.bzl", "internal_symbol")\n' +
            'load("@@internal_sdk//fuchsia:defs.bzl", "canonical_internal_symbol")\n' +
            'load("@rules_fuchsia//fuchsia:defs.bzl", "fuchsia_package")\n' +
            'load("@@rules_fuchsia//fuchsia/private:providers.bzl", "FuchsiaPackageInfo")\n' +
            _VIOLATION_TARGET +
            'filegroup(name = "canonical_sdk", deps = ["@@fuchsia_sdk//pkg/fdio"])\n' +
            'filegroup(name = "internal_sdk", deps = ["@internal_sdk//pkg/board-test-helper"])\n' +
            'filegroup(name = "canonical_internal", deps = ["@@internal_sdk//pkg/fake-mmio-reg"])\n' +
            'filegroup(name = "rules_pkg", deps = ["@rules_fuchsia//fuchsia"])\n' +
            'filegroup(name = "rules_target", deps = ["@rules_fuchsia//fuchsia:bzl_srcs"])\n' +
            'filegroup(name = "canonical_rules", deps = ["@@rules_fuchsia//fuchsia/workspace:templates"])\n'
        ),
        "src/defs.bzl": (
            'load("@rules_fuchsia//fuchsia:assembly.bzl", "fuchsia_board_configuration")\n' +
            'DEFAULT_DEP = "@fuchsia_sdk//pkg/syslog"\n'
        ),
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _msg("@fuchsia_sdk//fuchsia:defs.bzl"),
            filepath = "src/BUILD.bazel",
            line = 1,
            col = 7,
            end_col = 37,
        ),
        testing.finding(
            level = "error",
            message = _msg("@@fuchsia_sdk//fuchsia:defs.bzl"),
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 7,
            end_col = 38,
        ),
        testing.finding(
            level = "error",
            message = _msg("@internal_sdk//fuchsia:defs.bzl"),
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 7,
            end_col = 38,
        ),
        testing.finding(
            level = "error",
            message = _msg("@@internal_sdk//fuchsia:defs.bzl"),
            filepath = "src/BUILD.bazel",
            line = 4,
            col = 7,
            end_col = 39,
        ),
        testing.finding(
            level = "error",
            message = _msg("@rules_fuchsia//fuchsia:defs.bzl"),
            filepath = "src/BUILD.bazel",
            line = 5,
            col = 7,
            end_col = 39,
        ),
        testing.finding(
            level = "error",
            message = _msg("@@rules_fuchsia//fuchsia/private:providers.bzl"),
            filepath = "src/BUILD.bazel",
            line = 6,
            col = 7,
            end_col = 53,
        ),
        testing.finding(
            level = "error",
            message = _msg("@fuchsia_sdk//pkg/zx"),
            filepath = "src/BUILD.bazel",
            line = 7,
            col = 46,
            end_col = 66,
        ),
        testing.finding(
            level = "error",
            message = _msg("@@fuchsia_sdk//pkg/fdio"),
            filepath = "src/BUILD.bazel",
            line = 8,
            col = 44,
            end_col = 67,
        ),
        testing.finding(
            level = "error",
            message = _msg("@internal_sdk//pkg/board-test-helper"),
            filepath = "src/BUILD.bazel",
            line = 9,
            col = 43,
            end_col = 79,
        ),
        testing.finding(
            level = "error",
            message = _msg("@@internal_sdk//pkg/fake-mmio-reg"),
            filepath = "src/BUILD.bazel",
            line = 10,
            col = 49,
            end_col = 82,
        ),
        testing.finding(
            level = "error",
            message = _msg("@rules_fuchsia//fuchsia"),
            filepath = "src/BUILD.bazel",
            line = 11,
            col = 40,
            end_col = 63,
        ),
        testing.finding(
            level = "error",
            message = _msg("@rules_fuchsia//fuchsia:bzl_srcs"),
            filepath = "src/BUILD.bazel",
            line = 12,
            col = 43,
            end_col = 75,
        ),
        testing.finding(
            level = "error",
            message = _msg("@@rules_fuchsia//fuchsia/workspace:templates"),
            filepath = "src/BUILD.bazel",
            line = 13,
            col = 46,
            end_col = 90,
        ),
        testing.finding(
            level = "error",
            message = _msg("@rules_fuchsia//fuchsia:assembly.bzl"),
            filepath = "src/defs.bzl",
            line = 1,
            col = 7,
            end_col = 43,
        ),
        testing.finding(
            level = "error",
            message = _msg("@fuchsia_sdk//pkg/syslog"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 16,
            end_col = 40,
        ),
    ))

def test_disallowed_bazel_sdk_label_paths():
    res = _run_repository_paths_test({
        # Note: `//build/bazel_sdk/fuchsia_rules_common` is disallowed as an
        # in-tree `//build/bazel_sdk/...` label path (must use the
        # `@fuchsia_rules_common//` repository instead), and
        # `//build/bazel/bazel_sdk:...` is disallowed because any label path
        # containing `bazel_sdk` is prohibited in platform code:
        "src/BUILD.bazel": """
load("//build/bazel_sdk/bazel_rules_fuchsia/fuchsia:defs.bzl", "fuchsia_package")
load("@//build/bazel_sdk/bazel_rules_fuchsia/fuchsia:defs.bzl", "fuchsia_component")
load("@@//build/bazel_sdk/bazel_rules_fuchsia/fuchsia:defs.bzl", "fuchsia_cc_binary")
load("//build/bazel_sdk/fuchsia_rules_common:utils.bzl", "common_utils")
filegroup(name = "rules_target", deps = ["//build/bazel_sdk/bazel_rules_fuchsia:target"])
filegroup(name = "sdk_test", deps = ["@//build/bazel_sdk/tests:test"])
filegroup(name = "sdk_root", deps = ["@@//build/bazel_sdk:root"])
filegroup(name = "idk_generator", deps = ["//build/bazel/bazel_sdk:idk_to_bazel_sdk"])
filegroup(name = "common_dir", deps = ["//build/bazel_sdk/fuchsia_rules_common"])
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _msg("//build/bazel_sdk/bazel_rules_fuchsia/fuchsia:defs.bzl"),
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 7,
            end_col = 61,
        ),
        testing.finding(
            level = "error",
            message = _msg("@//build/bazel_sdk/bazel_rules_fuchsia/fuchsia:defs.bzl"),
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 7,
            end_col = 62,
        ),
        testing.finding(
            level = "error",
            message = _msg("@@//build/bazel_sdk/bazel_rules_fuchsia/fuchsia:defs.bzl"),
            filepath = "src/BUILD.bazel",
            line = 4,
            col = 7,
            end_col = 63,
        ),
        testing.finding(
            level = "error",
            message = _msg("//build/bazel_sdk/fuchsia_rules_common:utils.bzl"),
            filepath = "src/BUILD.bazel",
            line = 5,
            col = 7,
            end_col = 55,
        ),
        testing.finding(
            level = "error",
            message = _msg("//build/bazel_sdk/bazel_rules_fuchsia:target"),
            filepath = "src/BUILD.bazel",
            line = 6,
            col = 43,
            end_col = 87,
        ),
        testing.finding(
            level = "error",
            message = _msg("@//build/bazel_sdk/tests:test"),
            filepath = "src/BUILD.bazel",
            line = 7,
            col = 39,
            end_col = 68,
        ),
        testing.finding(
            level = "error",
            message = _msg("@@//build/bazel_sdk:root"),
            filepath = "src/BUILD.bazel",
            line = 8,
            col = 39,
            end_col = 63,
        ),
        testing.finding(
            level = "error",
            message = _msg("//build/bazel/bazel_sdk:idk_to_bazel_sdk"),
            filepath = "src/BUILD.bazel",
            line = 9,
            col = 44,
            end_col = 84,
        ),
        testing.finding(
            level = "error",
            message = _msg("//build/bazel_sdk/fuchsia_rules_common"),
            filepath = "src/BUILD.bazel",
            line = 10,
            col = 41,
            end_col = 79,
        ),
    ))

def test_allowed_repository_and_label_paths():
    res = _run_repository_paths_test({
        "src/BUILD.bazel": """
load("@fuchsia_rules_common//build_flags:build_flags.bzl", "build_flags")
load("@@fuchsia_rules_common//packages:resources.bzl", "fuchsia_resource")
load("@rules_fuchsia//common:repository_utils.bzl", "bazel_major_version_is_at_least")
load("@@rules_fuchsia//common/platforms:utils.bzl", "platform_utils")
load("//build/bazel/rules/cc:fx_cc_binary.bzl", "fx_cc_binary")

# Non-label strings containing `bazel_sdk` (such as target names or filenames
# without `//` or `@`) and `@rules_fuchsia//common` paths are allowed:
fx_cc_binary(
    name = "generate_bazel_sdk",
    srcs = ["idk_to_bazel_sdk.py"],
    deps = [
        "//sdk/lib/fdio",
        "@fuchsia_rules_common//packages:rebase_package_manifest",
        "@rules_fuchsia//common:toolchains",
    ],
)
""",
        "src/defs.bzl": '''
"""@fuchsia_sdk//pkg/zx and //build/bazel_sdk/foo in module docstring."""

def my_macro(dep):
    """@internal_sdk//pkg/foo in function docstring."""
    if dep == "@fuchsia_sdk//pkg/zx":
        fail("Do not depend on @fuchsia_sdk//pkg/zx or //build/bazel_sdk/foo")

my_rule = rule(
    doc = "@rules_fuchsia//fuchsia:defs.bzl",
    attrs = {
        "dep": attr.label(doc = "@fuchsia_sdk//pkg/zx"),
    },
    implementation = my_macro,
)
''',
    })
    asserts.eq(res.findings, ())

def test_policy_exempt_directories():
    res = _run_repository_paths_test({
        "build/bazel_sdk/bazel_rules_fuchsia/fuchsia/BUILD.bazel": _VIOLATION_TARGET,
        "build/bazel_sdk/tests/foo/BUILD.bazel": _VIOLATION_TARGET,
        "boards/foo/BUILD.bazel": _VIOLATION_TARGET,
        "examples/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
        "sdk/lib/driver/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/camera/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/connectivity/openthread/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/connectivity/wlan/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/devices/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/graphics/display/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/graphics/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/lib/ddk/BUILD.bazel": _VIOLATION_TARGET,
        "src/media/audio/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/media/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/ui/backlight/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/ui/input/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
        "src/ui/light/drivers/foo/BUILD.bazel": _VIOLATION_TARGET,
    })
    asserts.eq(res.findings, ())

def test_ignored_file_types():
    res = _run_repository_paths_test({
        "MODULE.bazel": _VIOLATION_TARGET,
        "WORKSPACE.bazel": _VIOLATION_TARGET,
    })
    asserts.eq(res.findings, ())

def run_repository_paths_tests():
    """Runs all repository_paths tests."""
    test_disallowed_sdk_repository_paths()
    test_disallowed_bazel_sdk_label_paths()
    test_allowed_repository_and_label_paths()
    test_policy_exempt_directories()
    test_ignored_file_types()
