# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Transitions used to build the IDK."""

load("@fuchsia_build_info//:args.bzl", "idk_buildable_api_levels", "idk_buildable_cpus", "target_cpu")
load("//build/bazel:fuchsia_api_level.bzl", "get_integer_for_api_level")

visibility(["//build/bazel/bazel_idk/tests/..."])

def _verify_is_main_platform_configuration(current_platforms, current_api_level):
    main_target_platform = "//build/bazel/platforms:fuchsia_platform_%s" % target_cpu

    # TODO(https://fxbug.dev/505802054): Remove once this is no longer being
    # invoked by a `bazel_action()` with the non-platform "fuchsia" platform.
    non_platform_main_target_platform = "//build/bazel/platforms:fuchsia_%s" % target_cpu

    if (len(current_platforms) != 1 or
        (str(current_platforms[0]) != ("@@" + main_target_platform) and
         str(current_platforms[0]) != ("@@" + non_platform_main_target_platform)) or
        current_api_level != "PLATFORM"):
        fail('This transition should only be used in the main "PLATFORM" build (platform: "%s", api_level: "PLATFORM"), not (platforms: "%s", api_level: "%s").' % (
            main_target_platform,
            current_platforms,
            current_api_level,
        ))

def _verify_is_main_platform_configuration_from_settings(settings):
    _verify_is_main_platform_configuration(
        settings["//command_line_option:platforms"],
        settings["@//build/bazel:fuchsia_api_level"],
    )

def _api_level_and_cpu_combinations_transition_impl(settings, _attr):
    _verify_is_main_platform_configuration_from_settings(settings)

    _platform_api_level_clang_switch = "-ffuchsia-api-level"

    # Remove any existing API level options.
    # TODO(https://fxbug.dev/443766378): Remove this once the Clang toolchain
    # automatically adds the flag based on `fuchsia_api_level`.
    base_copt = []
    for copt in settings["//command_line_option:copt"]:
        if copt.startswith(_platform_api_level_clang_switch + "="):
            continue
        base_copt.append(copt)

    combinations = []

    # In addition to the API levels supported by the IDK, we also build for
    # "PLATFORM" for CPU architectures other than `target_cpu` to populate
    # `arch/` in the IDK.
    # TODO(https://fxbug.dev/310006516): Remove `+ ["PLATFORM"]` once `arch/`
    # is removed from the IDK.
    for api_level in idk_buildable_api_levels + ["PLATFORM"]:
        for cpu in idk_buildable_cpus:
            if api_level == "PLATFORM" and cpu == target_cpu:
                # This configuration is already covered by the main platform build.
                continue

            combinations.append({
                "@//build/bazel:fuchsia_api_level": api_level,
                "//command_line_option:platforms": "@//build/bazel/platforms:fuchsia_platform_%s" % cpu,

                # TODO(https://fxbug.dev/443766378): Remove this once the Clang
                # toolchain automatically adds the flag based on `fuchsia_api_level`.
                "//command_line_option:copt": base_copt + ["%s=%s" % (_platform_api_level_clang_switch, get_integer_for_api_level(api_level))],
            })
    return combinations

# A 1:n transition to configurations for each combination of CPU architecture and API level.
api_level_and_cpu_combinations_transition = transition(
    implementation = _api_level_and_cpu_combinations_transition_impl,
    inputs = [
        "@//build/bazel:fuchsia_api_level",
        "//command_line_option:copt",
        "//command_line_option:platforms",
    ],
    outputs = [
        "@//build/bazel:fuchsia_api_level",
        "//command_line_option:copt",
        "//command_line_option:platforms",
    ],
)

def _build_in_all_idk_api_level_and_cpu_combinations_impl(ctx):
    if not ctx.attr.testonly:
        fail("This rule is only intended to be used in tests.")

    all_deps_depset = depset(direct = ctx.files.deps)

    return [
        DefaultInfo(files = all_deps_depset),
    ]

# Builds the specified targets for each combination of IDK buildable API level
# and CPU architecture.
build_in_all_idk_api_level_and_cpu_combinations = rule(
    implementation = _build_in_all_idk_api_level_and_cpu_combinations_impl,
    attrs = {
        "deps": attr.label_list(
            doc = "Targets to build for each combination of API level and CPU architecture.",
            cfg = api_level_and_cpu_combinations_transition,
        ),
    },
)
