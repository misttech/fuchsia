# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Sanitizers definitions for Clang.
"""

load("@rules_cc//cc:action_names.bzl", "ALL_CC_COMPILE_ACTION_NAMES", "ALL_CC_LINK_ACTION_NAMES")
load(
    "@rules_cc//cc:cc_toolchain_config_lib.bzl",
    "feature",
    "flag_group",
    "flag_set",
    "with_feature_set",
)

# This feature should only be enabled by one of the features below
# and will drop the optimization level and raise the debug info detail.
_clang_sanitizer_feature = feature(
    name = "clang_sanitizer",
    flag_sets = [
        flag_set(
            actions = ALL_CC_COMPILE_ACTION_NAMES,
            flag_groups = [
                flag_group(
                    flags = [
                        "-fno-omit-frame-pointer",
                        "-g3",
                        "-O1",
                    ],
                ),
            ],
        ),
    ],
)

def _sanitizer_mode_feature(name, mode):
    """Define a feature that corresponds to Clang -fsanitize=<mode>.

    The flag will be added both to compile and link actions, and all
    instances returned by this function will be mutually exclusive,
    to reflect Clang's own constraints.

    For example, when using '-fsanitize=address -fsanitize=hwaddress'
    Clang will complain with an error stating that this is not allowed.
    """
    return feature(
        name = name,
        flag_sets = [
            flag_set(
                actions = ALL_CC_COMPILE_ACTION_NAMES + ALL_CC_LINK_ACTION_NAMES,
                flag_groups = [flag_group(flags = ["-fsanitize=" + mode])],
            ),
        ],
        implies = ["clang_sanitizer"],

        # This ensures mutual exclusion between the different values returned
        # by this function. Bazel will print an error message. For example when
        # both `--features=asan` and `--features=hwasan` are used:
        #
        # ```
        # Analyzing: target //build/bazel/host_tests/cc_tests:static_test (58 packages loaded, 9 targets configured)
        # ERROR: ...../out/default/gen/build/bazel/workspace/build/bazel/host_tests/cc_tests/BUILD.bazel:12:11: in cc_library rule //build/bazel/host_tests/cc_tests:foo:
        # Traceback (most recent call last):
        #         File "/virtual_builtins_bzl/common/cc/cc_library.bzl", line 33, column 57, in _cc_library_impl
        #         File "/virtual_builtins_bzl/common/cc/cc_common.bzl", line 184, column 49, in _configure_features
        # Error in configure_features: Symbol clang_sanitizer_mode is provided by all of the following features: asan hwasan
        # ```
        provides = ["clang_sanitizer_mode"],
    )

# All these features correspond to mutually exclusive Clang sanitizer modes.
_asan_feature = _sanitizer_mode_feature("asan", "address")
_hwasan_feature = _sanitizer_mode_feature("hwasan", "hwaddress")
_msan_feature = _sanitizer_mode_feature("msan", "memory")
_tsan_feature = _sanitizer_mode_feature("tsan", "thread")

# The lsan feature corresponds to -fsanitize=leak which is compatible
# with either -fsanitize=address or -fsanitize=hwaddress, because their
# respective runtime (e.g. libclang_rt.asan.so) does implement the
# required support.
#
# To support this, only add the linker flag when neither asan or hwasan
# are enabled.
_lsan_feature = feature(
    name = "lsan",
    flag_sets = [
        flag_set(
            actions = ALL_CC_COMPILE_ACTION_NAMES,
            flag_groups = [flag_group(flags = ["-fsanitize=leak"])],
        ),
        flag_set(
            actions = ALL_CC_LINK_ACTION_NAMES,
            flag_groups = [flag_group(flags = ["-fsanitize=leak"])],
            with_features = [
                with_feature_set(
                    not_features = ["asan", "hwasan"],
                ),
            ],
        ),
    ],
    implies = ["clang_sanitizer"],
)

# The ubsan feature corresponds to -fsanitize=undefined at compile time
# and is also compatible with -fsanitize={address,hwaddress} for the same
# reasons as lsan, so implement a similar scheme.
_ubsan_feature = feature(
    name = "ubsan",
    flag_sets = [
        flag_set(
            actions = ALL_CC_COMPILE_ACTION_NAMES,
            flag_groups = [flag_group(flags = ["-fsanitize=undefined"])],
        ),
        flag_set(
            actions = ALL_CC_LINK_ACTION_NAMES,
            flag_groups = [flag_group(flags = ["-fsanitize=undefined"])],
            with_features = [
                with_feature_set(
                    not_features = ["asan", "hwasan"],
                ),
            ],
        ),
    ],
    implies = ["clang_sanitizer"],
)

sanitizer_features = [
    _clang_sanitizer_feature,
    _asan_feature,
    _hwasan_feature,
    _msan_feature,
    _tsan_feature,
    _lsan_feature,
    _ubsan_feature,
]
