# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule and provider for Fuchsia device tests."""

load(
    "@fuchsia_rules_common//:local_actions.bzl",
    "LOCAL_ONLY_ACTION_KWARGS",
)
load("@fuchsia_rules_common//:utils.bzl", "stub_executable")
load(
    "@fuchsia_rules_common//debug_symbols:providers.bzl",
    "FuchsiaDebugSymbolInfo",
)
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageInfo",
)
load(
    "//build/bazel/debug_symbols:aspects.bzl",
    "DebugSymbolManifestInfo",
    "collect_debug_symbols_manifest_aspect",
)
load(
    "//build/bazel/rules:current_platform_info.bzl",
    "CURRENT_PLATFORM_INFO_ATTRS",
    "get_current_platform_info",
)
load(
    "//build/bazel/rules/components:fx_component.bzl",
    "resolve_test_type_realm",
)
load(":fx_test_environment.bzl", "FxTestEnvironmentInfo")

# The repository that test packages are published to. This matches the default
# used by the GN `fuchsia_test_package()` template.
_PACKAGE_REPOSITORY = "fuchsia.com"

_FUCHSIA_TEST_INFO_FIELDS = {
    "test_label": "The canonical Bazel label of the fx_test() target. Used by " +
                  "`fx test` to rebuild the test package on demand.",
    "os": "The OS of the test, using Fuchsia conventions (always `fuchsia`).",
    "cpu": "The CPU of the test, using Fuchsia conventions.",
    "environments": "A list of environment dicts describing the target environments " +
                    "in which the test should run, or an empty list to use the build's " +
                    "default environments.",
    "build_only": "True if the test should only be built, not run.",
    "max_log_severity": "The maximum log severity allowed before the test fails.",
    # The fields below are specific to packaged component tests and will
    # become optional when `fx_test()` is extended to other target test
    # types (such as boot tests).
    "package_name": "The name of the Fuchsia package.",
    "package_manifest": "A File value for the package manifest. Its blob source paths are " +
                        "relative to the manifest itself, so it can be published from any " +
                        "working directory.",
    "test_components": "A list of structs describing each test component in the package, " +
                       "with `component_name`, `package_url`, and `realm` fields. `realm` " +
                       "is the non-hermetic test realm moniker to run the component in, or " +
                       "None if the test is hermetic.",
    "unstripped_binaries": "A depset of File values for the unstripped ELF binaries in the " +
                           "test package.",
}

def _fuchsia_test_info_init(**kwargs):
    # FuchsiaTestInfo.cquery reads every field, and reading an unset provider
    # field makes cquery skip the target while still exiting 0, so require
    # all fields up front to fail analysis instead.
    missing = [field for field in _FUCHSIA_TEST_INFO_FIELDS if field not in kwargs]
    if missing:
        fail("FuchsiaTestInfo is missing required fields: {}".format(", ".join(missing)))
    return kwargs

FuchsiaTestInfo, _new_fuchsia_test_info = provider(
    doc = "Provider for Bazel device tests visible to Fuchsia test runners (`fx test` and infra).",
    fields = _FUCHSIA_TEST_INFO_FIELDS,
    init = _fuchsia_test_info_init,
)

# `attr.label_list` can't tell an omitted `environments` apart from
# `environments = []`, so the default is this placeholder instead. That way an
# explicit `[]` (e.g. from a `select()`) fails rather than silently running the
# test in the default environments. The placeholder doesn't provide
# FxTestEnvironmentInfo, which is why `environments` checks providers here
# rather than with `providers = [...]`.
_DEFAULT_ENVIRONMENTS = Label("//build/bazel/rules/testing:default_environments")

def _collect_environments(ctx):
    if [t.label for t in ctx.attr.environments] == [_DEFAULT_ENVIRONMENTS]:
        return []
    if ctx.attr.build_only:
        fail("build_only tests should not specify environments")

    # GN's test_spec() rejects empty environments too.
    if not ctx.attr.environments:
        fail("`environments` is empty. Use `build_only = True` if the test " +
             "shouldn't run in this configuration, or omit `environments` to " +
             "use the default environments.")

    environments = []
    seen = {}
    for target in ctx.attr.environments:
        if FxTestEnvironmentInfo not in target:
            fail("`environments` entry {} is not an `fx_test_environment()`.".format(
                target.label,
            ))
        env = target[FxTestEnvironmentInfo].environment

        # build_tests_json.py dedupes too, but deduping here keeps
        # FuchsiaTestInfo readable when e.g. both `:emu_env` and `:aemu_env`
        # are listed.
        key = json.encode(env)
        if key not in seen:
            seen[key] = True
            environments.append(env)
    return environments

def _fx_test_impl(ctx):
    # Test rules default to `testonly = True`, so this only rejects an explicit
    # `testonly = False`, which would let non-test targets depend on this one.
    if not ctx.attr.testonly:
        fail("`fx_test()` targets are always testonly.")

    package_info = ctx.attr.package[FuchsiaPackageInfo]
    current_platform = get_current_platform_info(ctx)

    test_components = [
        component
        for component in package_info.packaged_components
        if component.component_info.is_test
    ]
    if not test_components:
        fail(
            "`fx_test()` requires `package` ({}) to have at least one ".format(
                ctx.attr.package.label,
            ) + "`test_components` entry.",
        )

    package_realm = resolve_test_type_realm(ctx.attr.test_type)
    test_component_entries = []
    for component in test_components:
        component_realm = getattr(component.component_info, "test_realm", None)
        if component_realm and package_realm and component_realm != package_realm:
            fail(
                "Conflicting `test_type` on `fx_test()` ({}) and `fx_test_component()` ({}) for component '{}'.".format(
                    package_realm,
                    component_realm,
                    component.component_info.name,
                ),
            )
        test_component_entries.append(
            struct(
                component_name = component.component_info.name,
                package_url = "fuchsia-pkg://{}/{}#{}".format(
                    _PACKAGE_REPOSITORY,
                    package_info.package_name,
                    component.dest,
                ),
                realm = component_realm or package_realm,
            ),
        )

    # The package manifest generated by `package-tool` refers to blobs with
    # paths relative to the Bazel execroot. Rebase them to be relative to the
    # manifest itself so that the manifest can be consumed from the Ninja build
    # directory, or from an infra test bot.
    #
    # TODO(https://fxbug.dev/564574581): This does not rebase the blob paths
    # within a subpackage's own manifest, so a test package with subpackages
    # can still only be published from the execroot.
    package_manifest = ctx.actions.declare_file(ctx.label.name + ".package_manifest.json")
    ctx.actions.run(
        outputs = [package_manifest],
        inputs = package_info.files,
        executable = ctx.executable._rebase_package_manifest,
        arguments = [
            "--package-manifest",
            package_info.package_manifest.path,
            "--updated-package-manifest",
            package_manifest.path,
        ],
        mnemonic = "RebaseTestPackageManifest",
        progress_message = "Rebasing package manifest for %{label}",
        **LOCAL_ONLY_ACTION_KWARGS
    )

    unstripped_binaries = ctx.attr.package[DebugSymbolManifestInfo].unstripped_binaries

    return [
        # Include `unstripped_binaries` in `DefaultInfo.files` so that building
        # an `fx_test()` target on demand (e.g. `fx build --fuchsia_platform`
        # during `fx test` or `fx coverage`) downloads the unstripped ELF
        # binaries under `--remote_download_outputs=toplevel` without running
        # `GenerateDebugSymbols`.
        DefaultInfo(
            files = depset(
                [package_manifest] + package_info.files,
                transitive = [unstripped_binaries],
            ),
            executable = stub_executable(ctx),
        ),
        FuchsiaTestInfo(
            test_label = ctx.label,
            os = current_platform.os,
            cpu = current_platform.cpu,
            environments = _collect_environments(ctx),
            build_only = ctx.attr.build_only,
            max_log_severity = ctx.attr.max_log_severity,
            package_name = package_info.package_name,
            package_manifest = package_manifest,
            test_components = test_component_entries,
            unstripped_binaries = unstripped_binaries,
        ),
        # Found by //build/bazel/debug_symbols:aspects.bzl so that
        # //build/bazel/target_tests can export the symbols to the GN build.
        ctx.attr.package[FuchsiaDebugSymbolInfo],
    ]

fx_test = rule(
    doc = """Exposes the test components of a Fuchsia package to the Fuchsia test runners (`fx test` and infra).

    Every component in the `package`'s `test_components` becomes a separate
    test, which is run on a Fuchsia device or emulator as
    `fuchsia-pkg://fuchsia.com/<package_name>#meta/<component_name>.cm`.

    Defining this target is not enough to make the tests visible to `fx test`
    and infra builders: the target must also be listed in a GN
    `bazel_test_suite()` target that is reachable from the build graph. See
    //docs/development/build/bazel_concepts/tests.md.

    This is a test rule so that these targets can be grouped with
    `test_suite()` and found with the `tests()` query function, exactly like
    `host_test()` targets. Its executable is a stub that always fails, because
    Fuchsia device tests cannot be run with `bazel test`; they must be run
    with `fx test` or by infra.

    Example usage:
    ```bazel
    fx_package(
        name = "pkg_tests_package",
        package_name = "pkg_tests",
        test_components = [":pkg_test_component"],
        ...
    )

    fx_test(
        name = "pkg_tests",
        package = ":pkg_tests_package",
        environments = [
            "//build/testing/environments:aemu_env",
            "//build/testing/environments:nuc11_env",
        ],
    )
    ```
    """,
    implementation = _fx_test_impl,
    test = True,
    attrs = {
        "package": attr.label(
            doc = "The `fx_package()` target containing the test components.",
            providers = [
                FuchsiaPackageInfo,
                FuchsiaDebugSymbolInfo,
            ],
            aspects = [collect_debug_symbols_manifest_aspect],
            mandatory = True,
        ),
        "environments": attr.label_list(
            doc = "The infra test environments that the test should run in, " +
                  "such as `//build/testing/environments:emu_env` or an " +
                  "`fx_test_environment()` target. If omitted, the test runs " +
                  "in the build's default test environments, exactly like GN " +
                  "tests that don't set `environments`.",
            default = [_DEFAULT_ENVIRONMENTS],
        ),
        "build_only": attr.bool(
            doc = "True if the test should only be built, not run. Environments must not be specified if this is true.",
            default = False,
        ),
        "max_log_severity": attr.string(
            doc = "The maximum log severity allowed before the test fails. Defaults to `WARN`.",
            default = "WARN",
            values = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR", "FATAL"],
        ),
        "test_type": attr.string(
            doc = "The non-hermetic test realm type to run the test components " +
                  "in (e.g. `starnix` or `system`). Can also be specified per " +
                  "component on `fx_test_component()`. Must be a key of " +
                  "`_TYPE_MONIKER_MAP` in //build/bazel/rules/components/fx_component.bzl. " +
                  "If omitted, tests run in the default hermetic test realm " +
                  "unless overridden on `fx_test_component()`. See " +
                  "https://fuchsia.dev/fuchsia-src/development/testing/components/test_runner_framework#non-hermetic_tests " +
                  "for valid types.",
        ),
        "_rebase_package_manifest": attr.label(
            default = "@fuchsia_rules_common//packages:rebase_package_manifest",
            executable = True,
            cfg = "exec",
        ),
    } | CURRENT_PLATFORM_INFO_ATTRS,
)
