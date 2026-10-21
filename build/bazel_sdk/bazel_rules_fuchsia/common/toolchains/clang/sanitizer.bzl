# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Sanitizers definitions for Clang.
"""

load("@bazel_skylib//lib:selects.bzl", "selects")
load("@rules_cc//cc:action_names.bzl", "ALL_CC_COMPILE_ACTION_NAMES", "ALL_CC_LINK_ACTION_NAMES")
load(
    "@rules_cc//cc:cc_toolchain_config_lib.bzl",
    "feature",
    "flag_group",
    "flag_set",
    "with_feature_set",
)
load(
    "//common:toolchains/clang/cc_features.bzl",
    "action_names",
)
load(
    "//common:toolchains/clang/feature_flag.bzl",
    "feature_flag",
)

# This feature should only be enabled by one of the features below
# and will drop the optimization level and raise the debug info detail.
def _get_clang_sanitizer_feature(use_default_build_flags = False):
    """Define the common clang_sanitizer feature implied by all sanitizer features."""

    # Frame pointers are required for sanitizers to capture stack traces on
    # allocations and errors (matching //build/config:frame_pointers).
    frame_pointer_flags = [
        "-fno-omit-frame-pointer",
        "-momit-leaf-frame-pointer",
    ]

    # NOTE: //build/config:default_optimize (-O1) is not ported to
    # @fuchsia_build_info//default_build_flags yet, so -O1 must still be
    # passed here in both modes. //build/config:default_debuginfo (-g3) is
    # already in @fuchsia_build_info//default_build_flags, so -g3 is only
    # passed in OOT mode.
    in_tree_remaining_compile_flags = [
        "-O1",
    ]
    oot_only_compile_flags = [] if use_default_build_flags else [
        "-g3",
    ]
    in_tree_remaining_link_flags = [
        "-O1",
    ]

    return feature(
        name = "clang_sanitizer",
        flag_sets = [
            flag_set(
                actions = ALL_CC_COMPILE_ACTION_NAMES,
                flag_groups = [
                    flag_group(
                        flags = frame_pointer_flags + in_tree_remaining_compile_flags + oot_only_compile_flags,
                    ),
                ],
            ),
            flag_set(
                actions = ALL_CC_LINK_ACTION_NAMES,
                flag_groups = [
                    flag_group(
                        flags = frame_pointer_flags + in_tree_remaining_link_flags,
                    ),
                ],
            ),
        ],
    )

def _sanitizer_mode_feature(
        name,
        mode,
        extra_compile_flags = [],
        extra_link_flags = [],
        enabled = False):
    """Define a feature that corresponds to Clang -fsanitize=<mode>.

    The flag will be added both to compile and link actions, and all
    instances returned by this function will be mutually exclusive,
    to reflect Clang's own constraints.

    For example, when using '-fsanitize=address -fsanitize=hwaddress'
    Clang will complain with an error stating that this is not allowed.
    """
    flag_sets = [
        flag_set(
            actions = ALL_CC_COMPILE_ACTION_NAMES + ALL_CC_LINK_ACTION_NAMES,
            flag_groups = [flag_group(flags = ["-fsanitize=" + mode])],
        ),
    ]
    if extra_compile_flags:
        flag_sets.append(
            flag_set(
                actions = ALL_CC_COMPILE_ACTION_NAMES,
                flag_groups = [flag_group(flags = extra_compile_flags)],
            ),
        )
    if extra_link_flags:
        flag_sets.append(
            flag_set(
                actions = ALL_CC_LINK_ACTION_NAMES,
                flag_groups = [flag_group(flags = extra_link_flags)],
            ),
        )

    return feature(
        name = name,
        enabled = enabled,
        flag_sets = flag_sets,
        implies = ["clang_sanitizer"],

        # This ensures mutual exclusion between the different values returned
        # by this function. Bazel will print an error message. For example when
        # both `--features=asan` and `--features=hwasan` are used:
        #
        # ```
        # Analyzing: target //build/bazel/rules/host_tests/tests/cc:static_test (58 packages loaded, 9 targets configured)
        # ERROR: ...../out/default/gen/build/bazel/workspace/build/bazel/rules/host_tests/tests/cc/BUILD.bazel:12:11: in cc_library rule //build/bazel/rules/host_tests/tests/cc:foo:
        # Traceback (most recent call last):
        #         File "/virtual_builtins_bzl/common/cc/cc_library.bzl", line 33, column 57, in _cc_library_impl
        #         File "/virtual_builtins_bzl/common/cc/cc_common.bzl", line 184, column 49, in _configure_features
        # Error in configure_features: Symbol clang_sanitizer_mode is provided by all of the following features: asan hwasan
        # ```
        provides = ["clang_sanitizer_mode"],
    )

def get_sanitizer_features(
        target_os,
        use_default_build_flags = False,
        enabled_features = []):
    """Return the list of Clang sanitizer feature() definitions for target_os.

    Args:
      target_os: Target OS, following Fuchsia conventions ("fuchsia", "linux").
      use_default_build_flags: True when building in-tree with
          @fuchsia_rules_common//build_flags:use_default_build_flags enabled,
          False in out-of-tree (OOT) SDK workspaces.
      enabled_features: Optional list of feature names to enable by default.

    Returns:
      A list of feature() objects.
    """
    is_fuchsia = target_os == "fuchsia"

    clang_sanitizer_feature = _get_clang_sanitizer_feature(use_default_build_flags)

    # NOTE: Unlike GN (which sets toolchain_variant.libprefix to "asan-ubsan/"
    # or "hwasan-ubsan/" when combined with ubsan), the IDK sysroot
    # (//sdk/lib/c/sysroot_entries.gni) and Bazel packaging rules
    # (clang_package_resources.bzl, generate_sdk_build_rules.bzl) only export
    # and package instrumented libraries under lib/asan/ and lib/hwasan/.
    # Until the IDK sysroot and packaging rules also support asan-ubsan and
    # hwasan-ubsan directories, always use asan/ld.so.1 and hwasan/ld.so.1.

    # LINT.IfChange(asan)
    asan_feature = _sanitizer_mode_feature(
        "asan",
        "address",
        extra_compile_flags = [
            "-mllvm",
            "-asan-use-odr-indicator=0",
            "-mllvm",
            "-asan-use-private-alias=1",
        ],
        extra_link_flags = [
            "-Wl,-dynamic-linker=asan/ld.so.1",
        ] if is_fuchsia else [],
        enabled = "asan" in enabled_features,
    )
    # LINT.ThenChange(//build/config/sanitizers/BUILD.gn:asan)

    hwasan_feature = _sanitizer_mode_feature(
        "hwasan",
        "hwaddress",
        extra_link_flags = [
            "-Wl,-dynamic-linker=hwasan/ld.so.1",
        ] if is_fuchsia else [],
        enabled = "hwasan" in enabled_features,
    )

    msan_feature = _sanitizer_mode_feature(
        "msan",
        "memory",
        enabled = "msan" in enabled_features,
    )

    # Out-of-tree SDK workspaces (when use_default_build_flags is False) only
    # export and package ld.so.1 under lib/, lib/asan/, and lib/hwasan/ (via
    # @fuchsia_clang//:novariant, :asan_variant, and :hwasan_variant). Passing
    # -Wl,-dynamic-linker={tsan,ubsan}/ld.so.1 out-of-tree would set
    # PT_INTERP to a path that does not exist in the package. Only pass these
    # flags in-tree (when use_default_build_flags is True) to match GN's
    # //build/config/sanitizers:variant-libprefix.
    tsan_feature = _sanitizer_mode_feature(
        "tsan",
        "thread",
        extra_link_flags = [
            "-Wl,-dynamic-linker=tsan/ld.so.1",
        ] if (is_fuchsia and use_default_build_flags) else [],
        enabled = "tsan" in enabled_features,
    )

    # The lsan feature corresponds to -fsanitize=leak which is compatible
    # with either -fsanitize=address or -fsanitize=hwaddress, because their
    # respective runtime (e.g. libclang_rt.asan.so) does implement the
    # required support.
    #
    # To support this, only add the linker flag when neither asan or hwasan
    # are enabled.
    lsan_feature = feature(
        name = "lsan",
        enabled = "lsan" in enabled_features,
        flag_sets = [
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
    # LINT.IfChange(ubsan)
    ubsan_c_family_flags = ["-fsanitize=undefined"]
    if not is_fuchsia:
        ubsan_c_family_flags.append("-fno-sanitize=vptr")

    ubsan_flag_sets = [
        flag_set(
            actions = ALL_CC_COMPILE_ACTION_NAMES + ALL_CC_LINK_ACTION_NAMES,
            flag_groups = [flag_group(flags = ubsan_c_family_flags)],
        ),
    ]
    if is_fuchsia and use_default_build_flags:
        # Only set ubsan/ld.so.1 in-tree (when use_default_build_flags is
        # True); out-of-tree SDK packaging only provides lib/ld.so.1 under
        # :novariant when neither asan nor hwasan is enabled.
        ubsan_flag_sets.append(
            flag_set(
                actions = ALL_CC_LINK_ACTION_NAMES,
                flag_groups = [flag_group(flags = ["-Wl,-dynamic-linker=ubsan/ld.so.1"])],
                with_features = [
                    with_feature_set(
                        not_features = ["asan", "hwasan"],
                    ),
                ],
            ),
        )

        # In-tree (when use_default_build_flags is True), UBSan's vptr check
        # requires RTTI (matching _ubsan_remove_common_configs removing
        # //build/config:no_rtti in //build/config/BUILDCONFIG.gn).
        #
        # Out-of-tree (when use_default_build_flags is False), prebuilt C++
        # libraries in @fuchsia_sdk are compiled with -fno-rtti, so enabling
        # -frtti in ubsan could cause undefined typeinfo symbol errors at link
        # time when deriving from polymorphic SDK classes.
        ubsan_flag_sets.append(
            flag_set(
                actions = action_names.all_cpp_compile_actions + ALL_CC_LINK_ACTION_NAMES,
                flag_groups = [flag_group(flags = ["-frtti"])],
            ),
        )

    ubsan_feature = feature(
        name = "ubsan",
        enabled = "ubsan" in enabled_features,
        flag_sets = ubsan_flag_sets,
        implies = ["clang_sanitizer"],
    )
    # LINT.ThenChange(//build/config/sanitizers/BUILD.gn:ubsan)

    # LINT.IfChange(sanitizer_features)
    # Note: _INSTRUMENTATION_FEATURES in fx_cc_binary.bzl includes these sanitizer
    # features plus "coverage" (defined in toolchain_utils.bzl).
    return [
        clang_sanitizer_feature,
        asan_feature,
        hwasan_feature,
        msan_feature,
        tsan_feature,
        lsan_feature,
        ubsan_feature,
    ]
    # LINT.ThenChange(//build/bazel/rules/cc/fx_cc_binary.bzl:instrumentation_features)

sanitizer_features = get_sanitizer_features("fuchsia", False)

# buildifier: disable=unnamed-macro
def define_clang_sanitizer_config_settings():
    """Create sanitizer-related feature_flag() and config_setting() targets.

    This function must be called from the BUILD.bazel of a given
    Clang external repository, it defines multiple config_setting()
    targets that other rules or functions depend on (e.g. package_resources.bzl)
    """

    # Holds True if --features=asan is used in the current build configuration
    # or through a target-specific `features = [...]` definition.
    feature_flag(
        name = "asan_feature_flag",
        feature_name = "asan",
        visibility = ["//visibility:private"],
    )

    # Holds True if --features=hwasan is used in the current build configuration
    # or through a target-specific `features = [...]` definition.
    feature_flag(
        name = "hwasan_feature_flag",
        feature_name = "hwasan",
        visibility = ["//visibility:private"],
    )

    # The following config_setting() determine which sanitizer mode
    # is enabled.
    #
    # IMPORTANT: The hwasan feature takes precedence over the asan one.
    # Keep this in sync with the definition of sanitizer_features in
    # //common:toolchains/clang/sanitizer.bzl

    # Holds True if no Asan features are enabled.
    native.config_setting(
        name = "novariant",
        flag_values = {
            ":asan_feature_flag": "False",
            ":hwasan_feature_flag": "False",
        },
        visibility = ["//visibility:public"],
    )

    # Holds True if 'asan' feature is enabled, but 'hwasan' is not.
    native.config_setting(
        name = "asan_variant",
        flag_values = {
            ":asan_feature_flag": "True",
            ":hwasan_feature_flag": "False",
        },
        visibility = ["//visibility:public"],
    )

    # Holds True if 'hwasan' feature is enabled.
    native.config_setting(
        name = "hwasan_variant",
        flag_values = {
            ":hwasan_feature_flag": "True",
            # ignore asan_feature_flag intentionally.
        },
        visibility = ["//visibility:public"],
    )

    # Cpu specific config_filters for novariant/asan/hwasan.
    native.config_setting(
        name = "arm64_build",
        constraint_values = ["@platforms//cpu:aarch64"],
    )
    native.config_setting(
        name = "x64_build",
        constraint_values = ["@platforms//cpu:x86_64"],
    )
    native.config_setting(
        name = "riscv64_build",
        constraint_values = ["@platforms//cpu:riscv64"],
    )
    #
    # Note that since that the SDK's fuchsia_transition() function
    # always changes --platforms, checking against @platforms//cpu:<name>
    # is enough here.
    #
    # The SDK Bazel rules used to also check against the command-line
    # --cpu value (also changed by the transition function), which
    # is not needed anymore since the introduction of --platforms
    # in Bazel 7.

    selects.config_setting_group(
        name = "arm64_novariant",
        match_all = [
            ":arm64_build",
            ":novariant",
        ],
    )

    selects.config_setting_group(
        name = "arm64_asan_variant",
        match_all = [
            ":arm64_build",
            ":asan_variant",
        ],
    )

    selects.config_setting_group(
        name = "arm64_hwasan_variant",
        match_all = [
            ":arm64_build",
            ":hwasan_variant",
        ],
    )

    selects.config_setting_group(
        name = "x64_novariant",
        match_all = [
            ":x64_build",
            ":novariant",
        ],
    )

    selects.config_setting_group(
        name = "x64_asan_variant",
        match_all = [
            ":x64_build",
            ":asan_variant",
        ],
    )

    selects.config_setting_group(
        name = "x64_hwasan_variant",
        match_all = [
            ":x64_build",
            ":hwasan_variant",
        ],
    )

    selects.config_setting_group(
        name = "riscv64_novariant",
        match_all = [
            ":riscv64_build",
            ":novariant",
        ],
    )

    selects.config_setting_group(
        name = "riscv64_asan_variant",
        match_all = [
            ":riscv64_build",
            ":asan_variant",
        ],
    )

    selects.config_setting_group(
        name = "riscv64_hwasan_variant",
        match_all = [
            ":riscv64_build",
            ":hwasan_variant",
        ],
    )
