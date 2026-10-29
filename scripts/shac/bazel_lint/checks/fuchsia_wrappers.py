# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks that Bazel files use Fuchsia-specific C/C++ and Rust rule wrappers."""

import dataclasses

from framework import (
    BazelCheck,
    CallInfo,
    FileContext,
    FileKind,
    LoadInfo,
    register_check,
)

_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#use-fuchsia-specific-wrappers"

_CC_MODULE_PREFIX = "@rules_cc//"
_RUST_MODULE_PREFIX = "@rules_rust//"

# Files that load from the Fuchsia Bazel SDK define SDK-based targets and must
# not load platform //build/bazel/rules/... wrappers.
_SDK_LOAD_PREFIXES = (
    "@fuchsia_sdk//",
    "@rules_fuchsia//fuchsia",
)


@dataclasses.dataclass(frozen=True)
class _WrapperSpec:
    wrapper_name: str
    wrapper_module: str
    disallowed_module_prefix: str


# TODO(https://fxbug.dev/570203009): Add `cc_shared_library`
# (`fx_cc_shared_library` from `//build/bazel/rules/cc:fx_cc_shared_library.bzl`)
# if https://fxrev.dev/1906761 lands before this change.
_WRAPPED_RULES: dict[str, _WrapperSpec] = {
    "cc_binary": _WrapperSpec(
        wrapper_name="fx_cc_binary",
        wrapper_module="//build/bazel/rules/cc:fx_cc_binary.bzl",
        disallowed_module_prefix=_CC_MODULE_PREFIX,
    ),
    "cc_library": _WrapperSpec(
        wrapper_name="fx_cc_library",
        wrapper_module="//build/bazel/rules/cc:fx_cc_library.bzl",
        disallowed_module_prefix=_CC_MODULE_PREFIX,
    ),
    # We don't run C++ tests directly, so there is no need to use cc_test().
    "cc_test": _WrapperSpec(
        wrapper_name="fx_cc_binary",
        wrapper_module="//build/bazel/rules/cc:fx_cc_binary.bzl",
        disallowed_module_prefix=_CC_MODULE_PREFIX,
    ),
    "rust_binary": _WrapperSpec(
        wrapper_name="rustc_binary",
        wrapper_module="//build/bazel/rules/rust:rustc_binary.bzl",
        disallowed_module_prefix=_RUST_MODULE_PREFIX,
    ),
    "rust_dylib_library": _WrapperSpec(
        wrapper_name="rustc_dylib",
        wrapper_module="//build/bazel/rules/rust:rustc_dylib.bzl",
        disallowed_module_prefix=_RUST_MODULE_PREFIX,
    ),
    "rust_library": _WrapperSpec(
        wrapper_name="rustc_library",
        wrapper_module="//build/bazel/rules/rust:rustc_library.bzl",
        disallowed_module_prefix=_RUST_MODULE_PREFIX,
    ),
    "rust_proc_macro": _WrapperSpec(
        wrapper_name="rustc_proc_macro",
        wrapper_module="//build/bazel/rules/rust:rustc_proc_macro.bzl",
        disallowed_module_prefix=_RUST_MODULE_PREFIX,
    ),
    "rust_test": _WrapperSpec(
        wrapper_name="rustc_test",
        wrapper_module="//build/bazel/rules/rust:rustc_test.bzl",
        disallowed_module_prefix=_RUST_MODULE_PREFIX,
    ),
}


def _loads_from_sdk(fc: FileContext) -> bool:
    """Returns True if `fc` loads from `@rules_fuchsia//fuchsia` or `@fuchsia_sdk//`."""
    return any(
        load.module_label.startswith(_SDK_LOAD_PREFIXES) for load in fc.loads
    )


def _resolve_wrapped_rule(fc: FileContext, call: CallInfo) -> str | None:
    """Returns the unwrapped rule name if `call` invokes a rule that has a Fuchsia wrapper."""
    if call.func_name in _WRAPPED_RULES:
        return call.func_name
    loaded = fc.loaded_symbols.get(call.func_name)
    if loaded is not None:
        module_label, exported_name = loaded
        spec = _WRAPPED_RULES.get(exported_name)
        if spec is not None and module_label.startswith(
            spec.disallowed_module_prefix
        ):
            return exported_name
    return None


@register_check
class FuchsiaWrappersCheck(BazelCheck):
    """Requires Fuchsia-specific C/C++ and Rust wrapper macros instead of upstream rules."""

    name = "bazel_fuchsia_wrappers"
    file_kinds = frozenset({FileKind.BUILD, FileKind.BZL})
    policy_exempt_globs = (
        # The Fuchsia C/C++ and Rust wrapper rules themselves load and invoke
        # the underlying @rules_cc and @rules_rust rules.
        "build/bazel/rules/cc/**",
        "build/bazel/rules/rust/**",
        # The Fuchsia Bazel SDK rules and tests cannot depend on platform
        # wrapper rules under //build/bazel/rules/. (Note: Other files that load
        # from `@fuchsia_sdk//` or `@rules_fuchsia//fuchsia` define SDK-based
        # targets and are exempted dynamically via `_loads_from_sdk()` below.)
        "build/bazel_sdk/bazel_rules_fuchsia/**",
        "build/bazel_sdk/tests/**",
    )
    supports_allowlist = True
    supports_targets_allowlist = True

    def check_load(self, fc: FileContext, load: LoadInfo) -> None:
        if _loads_from_sdk(fc):
            return
        for _local_name, exported_name, sym_node in load.symbols:
            spec = _WRAPPED_RULES.get(exported_name)
            if spec is not None and load.module_label.startswith(
                spec.disallowed_module_prefix
            ):
                fc.report(
                    f"Do not load `{exported_name}` from `{load.raw_module_label}`; "
                    f"load `{spec.wrapper_name}` from `{spec.wrapper_module}` instead.",
                    sym_node,
                    level="error",
                    url=_REFERENCE_URL,
                )

    def check_call(self, fc: FileContext, call: CallInfo) -> None:
        if _loads_from_sdk(fc):
            return
        rule_name = _resolve_wrapped_rule(fc, call)
        if rule_name is None:
            return
        spec = _WRAPPED_RULES[rule_name]
        fc.report(
            f"Do not use `{rule_name}()`; "
            f"use `{spec.wrapper_name}()` from `{spec.wrapper_module}` instead.",
            call.node,
            level="error",
            url=_REFERENCE_URL,
        )
