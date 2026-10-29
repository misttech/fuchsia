# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_fuchsia_wrappers."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_fuchsia_wrappers",
)
load("./base_test.star", "run_lint_test")

_DOCS_SUFFIX = "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#use-fuchsia-specific-wrappers."

_VIOLATION_CALL = 'cc_library(name = "unwrapped_cc_lib")\n'

def _run_fuchsia_wrappers_test(files):
    return run_lint_test(_bazel_fuchsia_wrappers, "bazel_fuchsia_wrappers", files)

def _load_msg(exported, module_label, wrapper, wrapper_module, alt_wrapper = None, alt_module = None):
    alt = (
        " (or `%s` from `%s` for Zircon libraries)" % (alt_wrapper, alt_module) if alt_wrapper and alt_module else ""
    )
    return (
        "Do not load `%s` from `%s`; load `%s` from `%s`%s instead. %s" % (
            exported,
            module_label,
            wrapper,
            wrapper_module,
            alt,
            _DOCS_SUFFIX,
        )
    )

def _call_msg(rule_name, wrapper, wrapper_module, alt_wrapper = None, alt_module = None):
    alt = (
        " (or `%s()` from `%s` for Zircon libraries)" % (alt_wrapper, alt_module) if alt_wrapper and alt_module else ""
    )
    return (
        "Do not use `%s()`; use `%s()` from `%s`%s instead. %s" % (
            rule_name,
            wrapper,
            wrapper_module,
            alt,
            _DOCS_SUFFIX,
        )
    )

def test_disallowed_cc_wrappers():
    res = _run_fuchsia_wrappers_test({
        "src/BUILD.bazel": (
            'load("@rules_cc//cc:defs.bzl", "cc_binary", "cc_library", "cc_test")\n' +
            'load("@@rules_cc//cc:cc_library.bzl", "cc_library")\n' +
            'cc_binary(name = "unwrapped_cc_bin")\n' +
            _VIOLATION_CALL +
            'cc_test(name = "unwrapped_cc_test")\n'
        ),
        "src/defs.bzl": """
load("@rules_cc//cc:defs.bzl", _cc_bin = "cc_binary", _cc_lib = "cc_library", _cc_test = "cc_test")
def aliased_cc_bin(): _cc_bin(name = "aliased_bin")
def aliased_cc_lib(): _cc_lib(name = "aliased_lib")
def aliased_cc_test(): _cc_test(name = "aliased_test")
def native_cc_bin(): native.cc_binary(name = "native_bin")
def native_cc_lib(): native.cc_library(name = "native_lib")
def native_cc_test(): native.cc_test(name = "native_test")
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _load_msg("cc_binary", "@rules_cc//cc:defs.bzl", "fx_cc_binary", "//build/bazel/rules/cc:fx_cc_binary.bzl"),
            filepath = "src/BUILD.bazel",
            line = 1,
            col = 32,
            end_col = 43,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("cc_library", "@rules_cc//cc:defs.bzl", "fx_cc_library", "//build/bazel/rules/cc:fx_cc_library.bzl", alt_wrapper = "cc_*_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/BUILD.bazel",
            line = 1,
            col = 45,
            end_col = 57,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("cc_test", "@rules_cc//cc:defs.bzl", "fx_cc_binary", "//build/bazel/rules/cc:fx_cc_binary.bzl"),
            filepath = "src/BUILD.bazel",
            line = 1,
            col = 59,
            end_col = 68,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("cc_library", "@@rules_cc//cc:cc_library.bzl", "fx_cc_library", "//build/bazel/rules/cc:fx_cc_library.bzl", alt_wrapper = "cc_*_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 39,
            end_col = 51,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_binary", "fx_cc_binary", "//build/bazel/rules/cc:fx_cc_binary.bzl"),
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 1,
            end_col = 37,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_library", "fx_cc_library", "//build/bazel/rules/cc:fx_cc_library.bzl", alt_wrapper = "cc_*_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/BUILD.bazel",
            line = 4,
            col = 1,
            end_col = 38,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_test", "fx_cc_binary", "//build/bazel/rules/cc:fx_cc_binary.bzl"),
            filepath = "src/BUILD.bazel",
            line = 5,
            col = 1,
            end_col = 36,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("cc_binary", "@rules_cc//cc:defs.bzl", "fx_cc_binary", "//build/bazel/rules/cc:fx_cc_binary.bzl"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 32,
            end_col = 53,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("cc_library", "@rules_cc//cc:defs.bzl", "fx_cc_library", "//build/bazel/rules/cc:fx_cc_library.bzl", alt_wrapper = "cc_*_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 55,
            end_col = 77,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("cc_test", "@rules_cc//cc:defs.bzl", "fx_cc_binary", "//build/bazel/rules/cc:fx_cc_binary.bzl"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 79,
            end_col = 99,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_binary", "fx_cc_binary", "//build/bazel/rules/cc:fx_cc_binary.bzl"),
            filepath = "src/defs.bzl",
            line = 3,
            col = 23,
            end_col = 52,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_library", "fx_cc_library", "//build/bazel/rules/cc:fx_cc_library.bzl", alt_wrapper = "cc_*_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/defs.bzl",
            line = 4,
            col = 23,
            end_col = 52,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_test", "fx_cc_binary", "//build/bazel/rules/cc:fx_cc_binary.bzl"),
            filepath = "src/defs.bzl",
            line = 5,
            col = 24,
            end_col = 55,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_binary", "fx_cc_binary", "//build/bazel/rules/cc:fx_cc_binary.bzl"),
            filepath = "src/defs.bzl",
            line = 6,
            col = 22,
            end_col = 59,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_library", "fx_cc_library", "//build/bazel/rules/cc:fx_cc_library.bzl", alt_wrapper = "cc_*_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/defs.bzl",
            line = 7,
            col = 22,
            end_col = 60,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_test", "fx_cc_binary", "//build/bazel/rules/cc:fx_cc_binary.bzl"),
            filepath = "src/defs.bzl",
            line = 8,
            col = 23,
            end_col = 59,
        ),
    ))

def test_disallowed_cc_shared_library():
    res = _run_fuchsia_wrappers_test({
        "src/BUILD.bazel": """
load("@rules_cc//cc:defs.bzl", "cc_shared_library")
cc_shared_library(name = "unwrapped_cc_shared_lib")
""",
        "src/defs.bzl": """
load("@@rules_cc//cc:cc_shared_library.bzl", _csl = "cc_shared_library")
def aliased_cc_shared_lib(): _csl(name = "aliased_shared_lib")
def native_cc_shared_lib(): native.cc_shared_library(name = "native_shared_lib")
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _load_msg("cc_shared_library", "@rules_cc//cc:defs.bzl", "fx_cc_shared_library", "//build/bazel/rules/cc:fx_cc_shared_library.bzl", alt_wrapper = "cc_shared_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 32,
            end_col = 51,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_shared_library", "fx_cc_shared_library", "//build/bazel/rules/cc:fx_cc_shared_library.bzl", alt_wrapper = "cc_shared_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 1,
            end_col = 52,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("cc_shared_library", "@@rules_cc//cc:cc_shared_library.bzl", "fx_cc_shared_library", "//build/bazel/rules/cc:fx_cc_shared_library.bzl", alt_wrapper = "cc_shared_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 46,
            end_col = 72,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_shared_library", "fx_cc_shared_library", "//build/bazel/rules/cc:fx_cc_shared_library.bzl", alt_wrapper = "cc_shared_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/defs.bzl",
            line = 3,
            col = 30,
            end_col = 63,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("cc_shared_library", "fx_cc_shared_library", "//build/bazel/rules/cc:fx_cc_shared_library.bzl", alt_wrapper = "cc_shared_library_zx", alt_module = "//build/bazel/rules:zx_library.bzl"),
            filepath = "src/defs.bzl",
            line = 4,
            col = 29,
            end_col = 81,
        ),
    ))

def test_disallowed_rust_wrappers():
    res = _run_fuchsia_wrappers_test({
        "src/BUILD.bazel": """
load("@rules_rust//rust:defs.bzl", "rust_binary", "rust_library", "rust_proc_macro", "rust_test")
rust_binary(name = "unwrapped_rust_bin")
rust_library(name = "unwrapped_rust_lib")
rust_proc_macro(name = "unwrapped_rust_macro")
rust_test(name = "unwrapped_rust_test")
""",
        "src/defs.bzl": """
load("@@rules_rust//rust:defs.bzl", _rb = "rust_binary", _rl = "rust_library", _rpm = "rust_proc_macro", _rt = "rust_test")
def aliased_rust_bin(): _rb(name = "aliased_bin")
def aliased_rust_lib(): _rl(name = "aliased_lib")
def aliased_rust_macro(): _rpm(name = "aliased_macro")
def aliased_rust_test(): _rt(name = "aliased_test")
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _load_msg("rust_binary", "@rules_rust//rust:defs.bzl", "rustc_binary", "//build/bazel/rules/rust:rustc_binary.bzl"),
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 36,
            end_col = 49,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("rust_library", "@rules_rust//rust:defs.bzl", "rustc_library", "//build/bazel/rules/rust:rustc_library.bzl"),
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 51,
            end_col = 65,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("rust_proc_macro", "@rules_rust//rust:defs.bzl", "rustc_proc_macro", "//build/bazel/rules/rust:rustc_proc_macro.bzl"),
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 67,
            end_col = 84,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("rust_test", "@rules_rust//rust:defs.bzl", "rustc_test", "//build/bazel/rules/rust:rustc_test.bzl"),
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 86,
            end_col = 97,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_binary", "rustc_binary", "//build/bazel/rules/rust:rustc_binary.bzl"),
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 1,
            end_col = 41,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_library", "rustc_library", "//build/bazel/rules/rust:rustc_library.bzl"),
            filepath = "src/BUILD.bazel",
            line = 4,
            col = 1,
            end_col = 42,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_proc_macro", "rustc_proc_macro", "//build/bazel/rules/rust:rustc_proc_macro.bzl"),
            filepath = "src/BUILD.bazel",
            line = 5,
            col = 1,
            end_col = 47,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_test", "rustc_test", "//build/bazel/rules/rust:rustc_test.bzl"),
            filepath = "src/BUILD.bazel",
            line = 6,
            col = 1,
            end_col = 40,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("rust_binary", "@@rules_rust//rust:defs.bzl", "rustc_binary", "//build/bazel/rules/rust:rustc_binary.bzl"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 37,
            end_col = 56,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("rust_library", "@@rules_rust//rust:defs.bzl", "rustc_library", "//build/bazel/rules/rust:rustc_library.bzl"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 58,
            end_col = 78,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("rust_proc_macro", "@@rules_rust//rust:defs.bzl", "rustc_proc_macro", "//build/bazel/rules/rust:rustc_proc_macro.bzl"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 80,
            end_col = 104,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("rust_test", "@@rules_rust//rust:defs.bzl", "rustc_test", "//build/bazel/rules/rust:rustc_test.bzl"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 106,
            end_col = 123,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_binary", "rustc_binary", "//build/bazel/rules/rust:rustc_binary.bzl"),
            filepath = "src/defs.bzl",
            line = 3,
            col = 25,
            end_col = 50,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_library", "rustc_library", "//build/bazel/rules/rust:rustc_library.bzl"),
            filepath = "src/defs.bzl",
            line = 4,
            col = 25,
            end_col = 50,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_proc_macro", "rustc_proc_macro", "//build/bazel/rules/rust:rustc_proc_macro.bzl"),
            filepath = "src/defs.bzl",
            line = 5,
            col = 27,
            end_col = 55,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_test", "rustc_test", "//build/bazel/rules/rust:rustc_test.bzl"),
            filepath = "src/defs.bzl",
            line = 6,
            col = 26,
            end_col = 52,
        ),
    ))

def test_disallowed_rust_dylib_library():
    res = _run_fuchsia_wrappers_test({
        "src/BUILD.bazel": """
load("@rules_rust//rust:defs.bzl", "rust_dylib_library")
rust_dylib_library(name = "unwrapped_rust_dylib")
""",
        "src/defs.bzl": """
load("@@rules_rust//rust:defs.bzl", _rdl = "rust_dylib_library")
def aliased_rust_dylib(): _rdl(name = "aliased_dylib")
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _load_msg("rust_dylib_library", "@rules_rust//rust:defs.bzl", "rustc_dylib", "//build/bazel/rules/rust:rustc_dylib.bzl"),
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 36,
            end_col = 56,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_dylib_library", "rustc_dylib", "//build/bazel/rules/rust:rustc_dylib.bzl"),
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 1,
            end_col = 50,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("rust_dylib_library", "@@rules_rust//rust:defs.bzl", "rustc_dylib", "//build/bazel/rules/rust:rustc_dylib.bzl"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 37,
            end_col = 64,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_dylib_library", "rustc_dylib", "//build/bazel/rules/rust:rustc_dylib.bzl"),
            filepath = "src/defs.bzl",
            line = 3,
            col = 27,
            end_col = 55,
        ),
    ))

def test_disallowed_rust_library_group():
    res = _run_fuchsia_wrappers_test({
        "src/BUILD.bazel": """
load("@rules_rust//rust:defs.bzl", "rust_library_group")
rust_library_group(name = "unwrapped_rust_lib_group")
""",
        "src/defs.bzl": """
load("@@rules_rust//rust:defs.bzl", _rlg = "rust_library_group")
def aliased_rust_lib_group(): _rlg(name = "aliased_lib_group")
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _load_msg("rust_library_group", "@rules_rust//rust:defs.bzl", "rustc_library_group", "//build/bazel/rules/rust:rustc_library_group.bzl"),
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 36,
            end_col = 56,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_library_group", "rustc_library_group", "//build/bazel/rules/rust:rustc_library_group.bzl"),
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 1,
            end_col = 54,
        ),
        testing.finding(
            level = "error",
            message = _load_msg("rust_library_group", "@@rules_rust//rust:defs.bzl", "rustc_library_group", "//build/bazel/rules/rust:rustc_library_group.bzl"),
            filepath = "src/defs.bzl",
            line = 2,
            col = 37,
            end_col = 64,
        ),
        testing.finding(
            level = "error",
            message = _call_msg("rust_library_group", "rustc_library_group", "//build/bazel/rules/rust:rustc_library_group.bzl"),
            filepath = "src/defs.bzl",
            line = 3,
            col = 31,
            end_col = 63,
        ),
    ))

def test_allowed_wrappers_and_unwrapped_symbols():
    res = _run_fuchsia_wrappers_test({
        "src/BUILD.bazel": """
load("@rules_cc//cc:defs.bzl", "CcInfo", "cc_common", "cc_import")
load("@rules_rust//crate_universe:defs.bzl", "crates_vendor")
load("//build/bazel/rules:zx_library.bzl", "cc_shared_library_zx", "cc_source_library_zx", "cc_static_library_zx")
load("//build/bazel/rules/cc:fx_cc_binary.bzl", "fx_cc_binary")
load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load("//build/bazel/rules/cc:fx_cc_shared_library.bzl", "fx_cc_shared_library")
load("//build/bazel/rules/rust:rustc_binary.bzl", "rustc_binary")
load("//build/bazel/rules/rust:rustc_dylib.bzl", "rustc_dylib")
load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")
load("//build/bazel/rules/rust:rustc_library_group.bzl", "rustc_library_group")
load("//build/bazel/rules/rust:rustc_proc_macro.bzl", "rustc_proc_macro")
load("//build/bazel/rules/rust:rustc_test.bzl", "rustc_test")

fx_cc_binary(name = "cc_bin")
fx_cc_library(name = "cc_lib")
fx_cc_shared_library(name = "cc_shared_lib")
cc_shared_library_zx(name = "zx_shared_lib")
cc_source_library_zx(name = "zx_source_lib")
cc_static_library_zx(name = "zx_static_lib")
cc_import(name = "imported")
rustc_binary(name = "rust_bin")
rustc_dylib(name = "rust_dylib")
rustc_library(name = "rust_lib")
rustc_library_group(name = "rust_lib_group")
rustc_proc_macro(name = "rust_macro")
rustc_test(name = "rust_unit_test")
crates_vendor(name = "vendor")
""",
        # Referencing `native.cc_library` as a symbol (without calling it) for
        # `inherit_attrs` on a symbolic macro is allowed:
        "src/defs.bzl": """
my_macro = macro(
    inherit_attrs = native.cc_library,
    implementation = _impl,
)
""",
    })
    asserts.eq(res.findings, ())

def test_sdk_files_skipped():
    res = _run_fuchsia_wrappers_test({
        "src/rules_fuchsia/BUILD.bazel": (
            'load("@rules_fuchsia//fuchsia:defs.bzl", "fuchsia_cc_binary")\n' +
            'load("@rules_cc//cc:defs.bzl", "cc_library")\n' +
            'load("@rules_rust//rust:defs.bzl", "rust_binary")\n' +
            _VIOLATION_CALL +
            'rust_binary(name = "sdk_rust_bin")\n'
        ),
        "src/fuchsia_sdk/BUILD.bazel": (
            'load("@@fuchsia_sdk//fuchsia:defs.bzl", "fuchsia_cc_binary")\n' +
            'load("@rules_cc//cc:defs.bzl", "cc_library")\n' +
            _VIOLATION_CALL
        ),
    })
    asserts.eq(res.findings, ())

def test_policy_exempt_directories():
    res = _run_fuchsia_wrappers_test({
        "build/bazel/rules/cc/fx_cc_library.bzl": _VIOLATION_CALL,
        "build/bazel/rules/rust/rustc_library.bzl": _VIOLATION_CALL,
        "build/bazel_sdk/bazel_rules_fuchsia/fuchsia/private/fuchsia_cc.bzl": _VIOLATION_CALL,
        "build/bazel_sdk/tests/foo/BUILD.bazel": _VIOLATION_CALL,
    })
    asserts.eq(res.findings, ())

def test_ignored_file_types():
    res = _run_fuchsia_wrappers_test({
        "MODULE.bazel": _VIOLATION_CALL,
        "WORKSPACE.bazel": _VIOLATION_CALL,
    })
    asserts.eq(res.findings, ())

def run_fuchsia_wrappers_tests():
    """Runs all fuchsia_wrappers tests."""
    test_disallowed_cc_wrappers()
    test_disallowed_cc_shared_library()
    test_disallowed_rust_wrappers()
    test_disallowed_rust_dylib_library()
    test_disallowed_rust_library_group()
    test_allowed_wrappers_and_unwrapped_symbols()
    test_sdk_files_skipped()
    test_policy_exempt_directories()
    test_ignored_file_types()
