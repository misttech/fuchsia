# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Starlark equivalents to //build/config/riscv64/riscv64.gni."""

load("@fuchsia_build_info//:args.bzl", "riscv64_enable_vector")
load("@fuchsia_rules_common//build_flags:compiler_build_flags.bzl", "compiler_build_flags")

# LINT.IfChange(riscv64_configs)

_RVA20_U64_MANDATORY_LETTERS = ["g", "c"]
_RVA20_U64_MANDATORY = []

_RVA22_U64_MANDATORY_LETTERS = _RVA20_U64_MANDATORY_LETTERS
_RVA22_U64_MANDATORY = _RVA20_U64_MANDATORY + [
    "zihintpause",
    "zba",
    "zbb",
    "zbs",
    "zicbom",
    "zicbop",
    "zicboz",
    "zfhmin",
    "zkt",
]

FUCHSIA_RISCV_IMPLICIT_FEATURE_LETTERS = []
FUCHSIA_RISCV_IMPLICIT_FEATURES = []

FUCHSIA_RISCV_EXTRA_FEATURE_LETTERS = ["v"] if riscv64_enable_vector else []
FUCHSIA_RISCV_EXTRA_FEATURES = []

FUCHSIA_RISCV_ABI = "lp64d"

def _dedup_append(base_list, extra_list):
    """Equivalent to GN's `list += extra; list -= extra; list += extra`."""
    return [item for item in base_list if item not in extra_list] + extra_list

def riscv64_abi_build_flags(
        name,
        *,
        abi = FUCHSIA_RISCV_ABI,
        exclude_letters = [],
        extra_features = [],
        extra_letters = [],
        visibility = None):
    """Macro wrapper around compiler_build_flags() matching GN's riscv64_abi_config().

    Assumes `toolchain_environment != "kernel"` in Bazel.

    Args:
        name: Target name passed to `compiler_build_flags()`.
        abi: Value for `-mabi=...` and `--target-abi=...`. Defaults to
            `FUCHSIA_RISCV_ABI` (`"lp64d"`).
        exclude_letters: List of RISC-V single-letter extension names to
            exclude from `-march=...` and disable via `-Ctarget-feature=-<ext>`.
        extra_features: Additional multi-letter extension names (e.g.,
            `"zicsr"`, `"zifencei"`) to append after mandatory profile features.
        extra_letters: Additional single-letter extension names to enable in
            `-march=...` and `-Ctarget-feature=+<ext>` (subject to removal via
            `exclude_letters`).
        visibility: Target visibility forwarded to `compiler_build_flags()`.
    """
    _extra_features = FUCHSIA_RISCV_IMPLICIT_FEATURES + FUCHSIA_RISCV_EXTRA_FEATURES
    _features = _dedup_append(_RVA22_U64_MANDATORY, _extra_features)
    if extra_features:
        _features = _dedup_append(_features, extra_features)

    _extra_feature_letters = (
        FUCHSIA_RISCV_IMPLICIT_FEATURE_LETTERS + FUCHSIA_RISCV_EXTRA_FEATURE_LETTERS
    )
    _letters = _dedup_append(_RVA22_U64_MANDATORY_LETTERS, _extra_feature_letters)
    if extra_letters:
        _letters = _dedup_append(_letters, extra_letters)

    _exclude_letters = list(exclude_letters)

    # Canonicalize single-letter extension ordering: g, i, m, a, c come first.
    # NOTE: Loop processes them in reverse order to get final result.
    for letter in ["c", "a", "m", "i", "g"]:
        if letter in _letters:
            _letters = [letter] + [x for x in _letters if x != letter]

    # Remove excluded letters.
    _letters = [x for x in _letters if x not in _exclude_letters]
    _exclude_letters = [x for x in _exclude_letters if x not in _letters]

    _march = "rv64" + "".join(_letters)
    for feature in _features:
        _march += "_" + feature

    c_family_flags = [
        "-march={}".format(_march),
        "-mabi={}".format(abi),
    ]

    all_rust_target_features = []
    for feature in _letters + _features:
        if feature == "g":  # Rust wants G exploded out, eliding I.
            all_rust_target_features.extend(["+m", "+a", "+f", "+d", "+zicsr"])
        else:
            all_rust_target_features.append("+{}".format(feature))

    for feature in _exclude_letters:
        if feature != "g":
            all_rust_target_features.append("-{}".format(feature))

    # LTO backend attributes include all features before eliding implicit ones, plus +relax.
    linker_llvm_flags = [
        "-mattr={}".format(attr)
        for attr in (all_rust_target_features + ["+relax"])
    ]

    base_implicit = ["+f", "+d", "+zfhmin"]
    fuchsia_implicit = base_implicit + ["+v"]

    fuchsia_features = [f for f in all_rust_target_features if f not in fuchsia_implicit]
    other_features = [f for f in all_rust_target_features if f not in base_implicit]

    compiler_build_flags(
        name = name,
        c_family_flags = c_family_flags,
        linker_llvm_flags = linker_llvm_flags,
        rust_llvm_flags = ["--target-abi={}".format(abi)],
        rust_target_features = select({
            "@platforms//os:fuchsia": fuchsia_features,
            "//conditions:default": other_features,
        }),
        visibility = visibility,
    )

# LINT.ThenChange(riscv64.gni:riscv64_configs)
