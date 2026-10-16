# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macros for defining a Fuchsia component and component manifest."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load(
    "@fuchsia_rules_common//components:component_manifest.bzl",
    "compile_component_manifest",
)
load(
    "@fuchsia_rules_common//components:fuchsia_component_common.bzl",
    "fuchsia_component_common",
)
load(
    "@fuchsia_rules_common//components:providers.bzl",
    "FuchsiaComponentManifestInfo",
)
load(
    "//build/bazel/aspects:component_manifests.bzl",
    "collect_component_manifests_aspect",
)
load(
    "//build/bazel/rules/components:expect_includes.bzl",
    "FuchsiaExpectedIncludesInfo",
)

# Maps `test_type` values to the realm moniker the test component runs in.
# LINT.IfChange(type_moniker_map)
_TYPE_MONIKER_MAP = {
    # keep-sorted start
    "bootstrap_driver_system": "/bootstrap/testing/driver-system-tests",
    "chromium": "/core/testing/chromium-tests",
    "component_framework": "/core/testing/component-framework-tests",
    "ctf": "/core/testing/ctf-tests",
    "device": "/core/testing/devices-tests",
    "driver_system": "/core/testing/driver-system-tests",
    "drm": "/core/testing/drm-tests",
    "starnix": "/core/testing/starnix-tests",
    "storage": "/core/testing/storage-tests",
    "system": "/core/testing/system-tests",
    "system_validation": "/core/testing/system-validation-tests",
    "test_arch": "/core/testing/test-arch-tests",
    "vfs_compliance": "/core/testing/vfs-compliance-tests",
    "vulkan": "/core/testing/vulkan-tests",
    # keep-sorted end
}
# LINT.ThenChange(//build/components/fuchsia_test_component.gni:type_moniker_map)

def resolve_test_type_realm(test_type):
    """Maps a `test_type` string to its test realm moniker.

    Args:
        test_type: A key of `_TYPE_MONIKER_MAP`, or an empty string or `None`
            for the default hermetic test realm.

    Returns:
        The test realm moniker, or `None` if `test_type` is not set.
    """
    if not test_type:
        return None
    if test_type not in _TYPE_MONIKER_MAP:
        fail(
            "Invalid `test_type` {}. Valid values are: {}".format(
                repr(test_type),
                ", ".join(sorted(_TYPE_MONIKER_MAP.keys())),
            ),
        )
    return _TYPE_MONIKER_MAP[test_type]

# `FuchsiaComponentManifestInfo` from `@fuchsia_rules_common` only carries the
# compiled `.cm` output file, whereas `cmc check-includes` must inspect the
# uncompiled `.cml` source file and its `includes` shards in the action sandbox.
# Because `fx_component_manifest` (which receives `manifest` and `includes`) and
# `fx_component` (which receives `deps` carrying `expect_includes()`) are
# separate targets, `fx_component_manifest` attaches this provider so
# `_fx_check_includes` can access the source `.cml` and its shard inputs.
_FxComponentManifestSourcesInfo = provider(
    doc = "Carries the source `.cml` file and `includes` shards from `fx_component_manifest` to `_fx_check_includes`.",
    fields = {
        "source_manifest": "The uncompiled `.cml` File passed to `fx_component_manifest`.",
        "includes": "A `depset[File]` of `.cml` shard files passed in `includes` to `fx_component_manifest`.",
    },
)

def _fx_component_manifest_impl(ctx):
    manifest_in = ctx.file.manifest
    component_name = ctx.attr.component_name or ctx.label.name

    cmc = ctx.executable._cmc_tool

    # Inject a `use` clause for `fuchsia.debugdata.Publisher` on coverage and
    # profile variants, matching `cm()` in //tools/cmc/build/cml.gni.
    # LINT.IfChange(debugdata_merge)
    debugdata_shard = None
    if ctx.attr._is_profile[BuildSettingInfo].value:
        debugdata_shard = ctx.file._profile_shard
    elif ctx.attr._is_coverage[BuildSettingInfo].value:
        debugdata_shard = ctx.file._coverage_shard

    include_paths = [".", "sdk/lib"]
    if debugdata_shard:
        include_paths.append(manifest_in.dirname)
        manifest_merged = ctx.actions.declare_file("{}_merge.cml".format(ctx.label.name))
        ctx.actions.run(
            executable = cmc,
            arguments = [
                "merge",
                "--output",
                manifest_merged.path,
                debugdata_shard.path,
                manifest_in.path,
            ],
            inputs = [
                debugdata_shard,
                manifest_in,
            ],
            outputs = [manifest_merged],
            mnemonic = "CmcMerge",
        )
        manifest_in = manifest_merged

    # LINT.ThenChange(//tools/cmc/build/cml.gni:debugdata_merge)

    return compile_component_manifest(
        ctx = ctx,
        cmc_tool = cmc,
        manifest_in = manifest_in,
        component_name = component_name,
        includes = ctx.files.includes,
        include_paths = include_paths,
    ) + [
        _FxComponentManifestSourcesInfo(
            source_manifest = ctx.file.manifest,
            includes = depset(ctx.files.includes),
        ),
    ]

fx_component_manifest = rule(
    doc = """Compiles a Fuchsia component manifest (.cml) into a binary component manifest (.cm).

This rule executes the component manifest compiler (cmc) tool to validate
and compile the input manifest file, including any specified dependency shards.
""",
    implementation = _fx_component_manifest_impl,
    attrs = {
        "manifest": attr.label(
            doc = "The component manifest file (.cml) to compile.",
            allow_single_file = [".cml"],
            mandatory = True,
        ),
        "component_name": attr.string(
            doc = "The name of the component. Defaults to the label name of the target.",
        ),
        "includes": attr.label_list(
            doc = """Other manifest shard files (.shard.cml or .cml) to include during compilation.
Explicitly setting this is necessary for sandboxed build action execution.""",
            allow_files = [".cml"],
        ),
        "_cmc_tool": attr.label(
            doc = "The path to the component manifest compiler (cmc) tool.",
            default = "//tools/cmc:cmc",
            executable = True,
            cfg = "exec",
        ),
        "_is_coverage": attr.label(
            default = "//build/bazel/config/variables:is_coverage",
        ),
        "_is_profile": attr.label(
            default = "//build/bazel/config/variables:is_profile",
        ),
        "_coverage_shard": attr.label(
            default = "//sdk/lib/sys/testing:coverage.shard.cml",
            allow_single_file = [".cml"],
        ),
        "_profile_shard": attr.label(
            default = "//build/config/profile:instrumentation_data.cml",
            allow_single_file = [".cml"],
        ),
    },
)

def _fx_check_includes_impl(ctx):
    enforced_shards = depset(
        transitive = [
            dep[FuchsiaExpectedIncludesInfo].enforced_shards
            for dep in ctx.attr.deps
            if FuchsiaExpectedIncludesInfo in dep
        ],
    )
    transitive_shards = depset(
        transitive = [
            dep[FuchsiaExpectedIncludesInfo].transitive_shards
            for dep in ctx.attr.deps
            if FuchsiaExpectedIncludesInfo in dep
        ],
    )
    manifest_info = ctx.attr.compiled_manifest[FuchsiaComponentManifestInfo]
    sources_info = ctx.attr.compiled_manifest[_FxComponentManifestSourcesInfo]
    enforced_list = enforced_shards.to_list()
    if not enforced_list:
        return [
            ctx.attr.compiled_manifest[DefaultInfo],
            manifest_info,
        ]

    manifest_in = sources_info.source_manifest
    stamp_file = ctx.actions.declare_file(ctx.label.name + ".stamp")

    args = [
        "--stamp",
        stamp_file.path,
        "check-includes",
        manifest_in.path,
    ] + [
        s.short_path
        for s in enforced_list
    ] + [
        "--includeroot",
        ".",
        "--includepath",
        manifest_in.dirname,
        "--includepath",
        "sdk/lib",
    ]

    ctx.actions.run(
        executable = ctx.executable._cmc_tool,
        arguments = args,
        inputs = depset(
            [manifest_in],
            transitive = [sources_info.includes, transitive_shards],
        ),
        outputs = [stamp_file],
        mnemonic = "CmcCheckIncludes",
    )

    return [
        ctx.attr.compiled_manifest[DefaultInfo],
        manifest_info,
        OutputGroupInfo(_validation = depset([stamp_file])),
    ]

_fx_check_includes = rule(
    doc = """Verifies that a component manifest includes all expected shards from its dependencies.

    Inspects `deps` via `collect_component_manifests_aspect` for
    `FuchsiaExpectedIncludesInfo`. If any `enforced_shards` are present, runs
    `cmc check-includes` on the source `.cml` manifest from `compiled_manifest`
    and registers the validation stamp in `OutputGroupInfo._validation`.
    """,
    implementation = _fx_check_includes_impl,
    attrs = {
        "compiled_manifest": attr.label(
            doc = "The `fx_component_manifest` target for the component.",
            providers = [
                FuchsiaComponentManifestInfo,
                _FxComponentManifestSourcesInfo,
            ],
            mandatory = True,
        ),
        "deps": attr.label_list(
            doc = "Dependencies of the component to inspect for expected includes.",
            aspects = [collect_component_manifests_aspect],
        ),
        "_cmc_tool": attr.label(
            doc = "The path to the component manifest compiler (cmc) tool.",
            default = "//tools/cmc:cmc",
            executable = True,
            cfg = "exec",
        ),
    },
    provides = [FuchsiaComponentManifestInfo],
)

_COMMON_COMPONENT_ATTRS = {
    # The behavior is different from that documented for the inherited attribute.
    "component_name": attr.string(
        doc = """The name of the component.

        This value will override the component name value in the `compiled_manifest`.
        Defaults to the `name` of this target.
        """,
        mandatory = False,
    ),
    "check_includes": attr.bool(
        doc = """Whether to check against `expect_includes()` in `deps`.

        If `True`, verifies via `cmc check-includes` that the component manifest
        includes all manifest shards required by `expect_includes()` targets in
        its transitive dependencies.
        """,
        default = True,
        configurable = False,
    ),

    # Set by each macro via `_is_test_component` instead: use
    # `fx_test_component()` for test components.
    "is_test": None,

    # Set from `test_type`, which only `fx_test_component()` accepts.
    "test_realm": None,

    # TODO(https://fxbug.dev/520207779): Determine whether we need these attributes for platform
    # packages and update the implementation and `fuchsia_component_common()`'s documentation of
    # these attributes as appropriate.
    "moniker": None,
    "is_driver": None,
}

def _fx_component_impl(
        name,
        component_name,
        compiled_manifest,
        deps,
        check_includes,
        testonly,
        visibility,
        _is_test_component,
        # Only `fx_test_component()` declares this attribute.
        test_type = None,
        **kwargs):
    if _is_test_component:
        # Inherited attributes that are not set default to None, so only an
        # explicit `testonly = False` is an error.
        # See https://bazel.build/extending/macros#attribute-inheritance.
        if testonly != None and not testonly:
            fail("`fx_test_component()` targets are always testonly.")
        testonly = True

    if check_includes:
        check_includes_name = name + ".check_includes"
        _fx_check_includes(
            name = check_includes_name,
            compiled_manifest = compiled_manifest,
            deps = deps,
            testonly = testonly,
            tags = (kwargs.get("tags") or []) + ["manual"],
        )
        compiled_manifest = ":" + check_includes_name

    fuchsia_component_common(
        name = name,
        compiled_manifest = compiled_manifest,
        component_name = component_name or name,
        deps = deps,
        testonly = testonly,
        visibility = visibility,

        # Attributes not supported by the in-tree macro.
        moniker = None,
        is_driver = False,

        # Required by `fx_package(test_components = ...)` and used by `fx_test()`.
        is_test = _is_test_component,
        test_realm = resolve_test_type_realm(test_type),

        # Forward extra attributes.
        **kwargs
    )

def _component_attrs(*, is_test_component):
    return _COMMON_COMPONENT_ATTRS | {
        # Private, so callers can't override which macro they're using, and
        # non-configurable so the impl gets a plain bool rather than an
        # always-truthy select().
        "_is_test_component": attr.bool(
            default = is_test_component,
            configurable = False,
        ),
    }

fx_component = macro(
    doc = """Creates a Fuchsia component which can be added to a package.

This macro will take a component manifest and compile it into a form that
is suitable to be included in a package. The component can include any
number of dependencies which will be included in the final package.
""",
    implementation = _fx_component_impl,
    inherit_attrs = fuchsia_component_common,
    attrs = _component_attrs(is_test_component = False),
)

fx_test_component = macro(
    doc = """Creates a Fuchsia test component which can be added to an `fx_package()`.

This is the test equivalent of `fx_component()`: the resulting component is
always testonly, and is marked as a test component, which means it must be
listed in the `test_components` attribute of `fx_package()` (and not in
`components`).
""",
    implementation = _fx_component_impl,
    inherit_attrs = fuchsia_component_common,
    attrs = _component_attrs(is_test_component = True) | {
        "test_type": attr.string(
            doc = """The non-hermetic test realm type to run the test component in (e.g. `"starnix"` or `"system"`).

            Must be a key of `_TYPE_MONIKER_MAP` in
            //build/bazel/rules/components/fx_component.bzl, which maps it to
            the test realm moniker. If omitted, the test runs in the default
            hermetic test realm.
            See https://fuchsia.dev/fuchsia-src/development/testing/components/test_runner_framework#non-hermetic_tests
            for valid types.
            """,
            configurable = False,
        ),
    },
)
