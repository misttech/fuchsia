# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules for verifying the outputs and providers of Bazel test rules."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load(
    "//build/bazel/aspects:component_manifests.bzl",
    "collect_component_manifests_aspect",
)
load("//build/bazel/aspects:runner.bzl", "FuchsiaRunnerShardInfo")
load(
    "//build/bazel/debug_symbols:aspects.bzl",
    "DebugSymbolManifestInfo",
    "collect_debug_symbols_manifest_aspect",
)
load("//build/bazel/rules/testing:fx_test.bzl", "FuchsiaTestInfo")

def _verify_file_path_impl(ctx):
    files_list = ctx.attr.target[DefaultInfo].files.to_list()
    if len(files_list) < 1:
        fail("The target's `DefaultInfo` must have at least one file.")

    actual_file_path = files_list[0].short_path
    if actual_file_path != ctx.attr.expected_file_path:
        fail("The actual file path (`%s`) does not match the expected file path (`%s`)." %
             (actual_file_path, ctx.attr.expected_file_path))
    return []

verify_file_path = rule(
    doc = "Verifies that the actual file path of a target matches the expected short file path." +
          "Compares the short file path of the first file in the `target`'s " +
          "`[DefaultInfo].files` with the `expected_file_path` attribute. " +
          "The `target` must have at least one file in `DefaultInfo.files`.",
    implementation = _verify_file_path_impl,
    attrs = {
        "target": attr.label(mandatory = True),
        "expected_file_path": attr.string(mandatory = True),
    },
)

def _verify_fx_test_environments_impl(ctx):
    actual = ctx.attr.test[FuchsiaTestInfo].environments
    expected = [json.decode(env) for env in ctx.attr.expected_environments]
    if actual != expected:
        fail(
            "The actual environments (`%s`) do not match the expected environments (`%s`)." %
            (actual, expected),
        )
    actual_build_only = ctx.attr.test[FuchsiaTestInfo].build_only
    if actual_build_only != ctx.attr.expected_build_only:
        fail(
            "The actual build_only (`%s`) does not match the expected build_only (`%s`)." %
            (actual_build_only, ctx.attr.expected_build_only),
        )
    return []

_verify_fx_test_environments = rule(
    doc = "Verifies that the `environments` and `build_only` fields of a target's `FuchsiaTestInfo` match the expected values.",
    implementation = _verify_fx_test_environments_impl,
    attrs = {
        "test": attr.label(
            mandatory = True,
            providers = [FuchsiaTestInfo],
        ),
        "expected_environments": attr.string_list(mandatory = True),
        "expected_build_only": attr.bool(default = False),
    },
)

def verify_fx_test_environments(name, test, expected_environments, expected_build_only = False, **kwargs):
    """Verifies that an `fx_test()` target's `FuchsiaTestInfo.environments` matches `expected_environments`.

    Args:
        name: The target name.
        test: The `fx_test()` target to verify.
        expected_environments: The expected list of environment dicts.
        expected_build_only: The expected value of `build_only`.
        **kwargs: Common rule attributes (e.g. `testonly`).
    """
    _verify_fx_test_environments(
        name = name,
        test = test,
        expected_environments = [json.encode(env) for env in expected_environments],
        expected_build_only = expected_build_only,
        **kwargs
    )

def _verify_fx_test_realms_impl(ctx):
    test_info = ctx.attr.test[FuchsiaTestInfo]
    actual_realms = {
        c.component_name: c.realm or ""
        for c in test_info.test_components
    }
    if actual_realms != ctx.attr.expected_realms:
        fail(
            "Expected test component realms {}, got {}.".format(
                ctx.attr.expected_realms,
                actual_realms,
            ),
        )
    return []

verify_fx_test_realms = rule(
    doc = "Verifies the `realm` values on `FuchsiaTestInfo.test_components` of an `fx_test()` target.",
    implementation = _verify_fx_test_realms_impl,
    attrs = {
        "test": attr.label(
            providers = [FuchsiaTestInfo],
            mandatory = True,
        ),
        "expected_realms": attr.string_dict(
            doc = "Map of `component_name` to expected `realm` moniker (or empty string if hermetic).",
            mandatory = True,
        ),
    },
)

def _verify_fx_test_unstripped_binaries_impl(ctx):
    test_info = ctx.attr.test[FuchsiaTestInfo]
    unstripped_files = test_info.unstripped_binaries.to_list()
    actual = sorted([f.basename for f in unstripped_files])
    expected = sorted(ctx.attr.expected_basenames)
    if actual != expected:
        fail(
            "Expected unstripped_binaries {}, got {}.".format(
                expected,
                actual,
            ),
        )
    packaged_dest_basenames = sorted([
        unstripped_file.basename
        for unstripped_file, _ in ctx.attr.test[DebugSymbolManifestInfo].dest_paths.to_list()
        if not unstripped_file.owner.repo_name
    ])
    if packaged_dest_basenames != expected:
        fail(
            "Expected packaged FuchsiaUnstrippedBinariesInfo dest_paths basenames {}, got {}.".format(
                expected,
                packaged_dest_basenames,
            ),
        )
    default_files = {f: True for f in ctx.attr.test[DefaultInfo].files.to_list()}
    missing = [f.short_path for f in unstripped_files if f not in default_files]
    if missing:
        fail(
            "Expected unstripped_binaries {} to be in DefaultInfo.files of {}.".format(
                missing,
                ctx.attr.test.label,
            ),
        )
    return []

verify_fx_test_unstripped_binaries = rule(
    doc = "Verifies `FuchsiaTestInfo.unstripped_binaries`, packaged `dest_paths`, and `DefaultInfo.files` of an `fx_test()` target.",
    implementation = _verify_fx_test_unstripped_binaries_impl,
    attrs = {
        "test": attr.label(
            providers = [FuchsiaTestInfo],
            aspects = [collect_debug_symbols_manifest_aspect],
            mandatory = True,
        ),
        "expected_basenames": attr.string_list(
            doc = "Expected basenames of the unstripped binaries.",
            mandatory = True,
        ),
    },
)

def _verify_runner_shard_impl(ctx):
    shards = [
        "//" + f.short_path
        for f in ctx.attr.target[FuchsiaRunnerShardInfo].shards.to_list()
    ]
    if shards != [ctx.attr.expected_shard]:
        fail(
            "Expected runner shard [{}], got {}.".format(
                ctx.attr.expected_shard,
                shards,
            ),
        )
    return []

verify_runner_shard = rule(
    doc = "Verifies that `target` propagates `expected_shard` in `FuchsiaRunnerShardInfo`.",
    implementation = _verify_runner_shard_impl,
    attrs = {
        "target": attr.label(
            aspects = [collect_component_manifests_aspect],
            mandatory = True,
        ),
        "expected_shard": attr.string(
            mandatory = True,
        ),
    },
)

def _analysis_failure_test_impl(ctx):
    env = analysistest.begin(ctx)
    asserts.expect_failure(env, ctx.attr.expected_message)

    # analysistest normally reports failures only when the test is run, but
    # these tests are built rather than run (see `build_only_tests`), so fail
    # analysis directly instead.
    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

analysis_failure_test = analysistest.make(
    _analysis_failure_test_impl,
    doc = "Verifies that `target_under_test` fails analysis with an error containing `expected_message`. " +
          "The target under test should be tagged `manual` so that wildcard builds skip it.",
    expect_failure = True,
    attrs = {
        "expected_message": attr.string(mandatory = True),
    },
)
