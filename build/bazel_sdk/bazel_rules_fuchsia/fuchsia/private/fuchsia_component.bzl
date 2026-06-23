# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load(
    "@fuchsia_rules_common//:utils.bzl",
    "make_resource_struct",
)
load(
    "@fuchsia_rules_common//debug_symbols:debug_symbols.bzl",
    "merge_debug_symbol_infos",
)

# buildifier: disable=module-docstring
load(":fuchsia_component_manifest.bzl", "ensure_compiled_component_manifest")
load(
    ":providers.bzl",
    "FuchsiaComponentInfo",
    "FuchsiaComponentManifestInfo",
    "FuchsiaPackageResourcesInfo",
)

def _manifest_target(name, manifest_in, tags, testonly):
    target_name = name + "_ensure_compiled_manifest"
    ensure_compiled_component_manifest(
        name = target_name,
        dep = manifest_in,
        testonly = testonly,
        tags = tags + ["manual"],
    )
    return target_name

def fuchsia_component(
        *,
        name,
        manifest,
        moniker = "/core/ffx-laboratory:{COMPONENT_NAME}",
        component_name = None,
        deps = [],
        tags = ["manual"],
        **kwargs):
    """Creates a Fuchsia component that can be added to a package.

    Args:
        name: The target name.
        manifest: The component manifest file.
            This attribute can be a `fuchsia_component_manifest` target or a `.cml`
            file. If a `.cml` file is provided it will be compiled into a `.cm` file.
            If `component_name` is provided, the generated `.cm` file will
            inherit that name. Otherwise, it will keep the same basename.
            TODO(http://b/525461025): Implement the `component_name` behavior for the `.cm` file.

            If you need to have more control over the compilation of the `.cm` file
            we suggest you create a `fuchsia_component_manifest` target and pass
            it to this rule.
        moniker: The moniker to run the component under.
            Defaults to "/core/ffx-laboratory:{COMPONENT_NAME}".
        component_name: The name of the component.
            Defaults to the component manifest file's basename.
        deps: A list of targets that this component depends on.
        tags: Typical meaning in Bazel. By default this target is manual.
        **kwargs: Extra attributes to forward to the build rule.
    """

    # Prevent callers from passing through these attributes, which are set appropriately below.
    for attr in ["is_driver", "is_test"]:
        if attr in kwargs:
            fail("Attribute `%s` is not supported. Use the appropriate macro instead." % attr)

    manifest_target = _manifest_target(name, manifest, tags, testonly = False)

    _fuchsia_component(
        name = name,
        moniker = moniker,
        compiled_manifest = manifest_target,
        component_name = component_name,
        deps = deps,
        tags = tags,
        is_driver = False,
        is_test = False,
        **kwargs
    )

def fuchsia_test_component(
        *,
        name,
        manifest,
        component_name = None,
        deps = [],
        tags = ["manual"],
        **kwargs):
    """Creates a Fuchsia component that can be added to a test package.

    See `fuchsia_component` for more information.

    Args:
        name: The target name.
        manifest: The component manifest file.
        component_name: The name of the component.
        deps: A list of targets that this component depends on.
        tags: Typical meaning in Bazel. By default this target is manual.
        **kwargs: Extra attributes to forward to the build rule.
    """

    # Prevent passing through these attributes, which are set appropriately below.
    for attr in ["moniker", "is_driver", "is_test"]:
        if attr in kwargs:
            fail("Attribute `%s` is not supported." % attr)

    manifest_target = _manifest_target(name, manifest, tags, testonly = True)

    _fuchsia_component(
        name = name,
        moniker = None,
        compiled_manifest = manifest_target,
        component_name = component_name,
        deps = deps,
        tags = tags,
        is_driver = False,
        is_test = True,
        testonly = True,
        **kwargs
    )

def fuchsia_driver_component(
        # TODO(http://b/525461025): Add `*,` here like the peer macros.
        name,
        manifest,
        driver_lib,
        bind_bytecode,
        component_name = None,
        deps = [],
        tags = ["manual"],
        **kwargs):
    """Creates a Fuchsia component that can be registered as a driver.

    See `fuchsia_component` for more information.

    Args:
        name: The target name.
        manifest: The component manifest file.
        driver_lib: The shared library that will be registered with the driver manager.
           This file will end up in /driver/<lib_name> and should match what is listed
           in the manifest. See https://fuchsia.dev/fuchsia-src/concepts/components/v2/driver_runner
           for more details.
        bind_bytecode: The driver bind bytecode needed for binding the driver.
        component_name: The name of the component.
        deps: A list of targets that this component depends on.
        tags: Typical meaning in Bazel. By default this target is manual.
        **kwargs: Extra attributes to forward to the build rule.
    """

    # Prevent passing through these attributes, which are set appropriately below.
    for attr in ["moniker", "is_driver", "is_test"]:
        if attr in kwargs:
            fail("Attribute `%s` is not supported." % attr)

    manifest_target = _manifest_target(name, manifest, tags, testonly = False)

    _fuchsia_component(
        name = name,
        moniker = None,
        compiled_manifest = manifest_target,
        component_name = component_name,
        deps = deps + [
            bind_bytecode,
            driver_lib,
        ],
        tags = tags,
        is_driver = True,
        is_test = False,
        **kwargs
    )

def _make_fuchsia_component_providers(*, component_name, manifest, resources, is_driver, is_test, moniker, run_tag):
    return [
        FuchsiaComponentInfo(
            name = component_name,
            manifest = manifest,
            resources = resources,
            is_driver = is_driver,
            is_test = is_test,
            moniker = moniker,
            run_tag = run_tag,
        ),
        FuchsiaPackageResourcesInfo(resources = [
            make_resource_struct(
                src = manifest,
                dest = "meta/{}".format(manifest.basename),
            ),
        ]),
    ]

def _fuchsia_component_impl(ctx):
    # TODO(http://b/525461025): Also check for session components per the doc string.
    if ctx.attr.moniker and (ctx.attr.is_driver or ctx.attr.is_test):
        fail("`moniker` should not be set for driver or test components.")

    component_name = ctx.attr.component_name or ctx.attr.compiled_manifest[FuchsiaComponentManifestInfo].component_name
    manifest = ctx.attr.compiled_manifest[FuchsiaComponentManifestInfo].compiled_manifest

    resources = []
    for dep in ctx.attr.deps:
        if FuchsiaPackageResourcesInfo in dep:
            resources += dep[FuchsiaPackageResourcesInfo].resources
        else:
            for mapping in dep[DefaultInfo].default_runfiles.root_symlinks.to_list():
                resources.append(make_resource_struct(src = mapping.target_file, dest = mapping.path))

            for f in dep.files.to_list():
                resources.append(make_resource_struct(src = f, dest = f.short_path))

    return _make_fuchsia_component_providers(
        component_name = component_name,
        manifest = manifest,
        resources = resources,
        is_driver = ctx.attr.is_driver,
        is_test = ctx.attr.is_test,
        moniker = ctx.attr.moniker.format(COMPONENT_NAME = component_name),
        run_tag = ctx.label.name,
    ) + [
        merge_debug_symbol_infos(ctx.attr.deps),
    ]

_fuchsia_component = rule(
    doc = """Creates a Fuchsia component which can be added to a package

This rule will take a component manifest and compile it into a form that
is suitable to be included in a package. The component can include any
number of dependencies which will be included in the final package.
""",
    implementation = _fuchsia_component_impl,
    attrs = {
        "deps": attr.label_list(
            doc = """A list of targets that this component depends on.

            The necessary files for each target will be included in the final package.
            """,
        ),
        "moniker": attr.string(
            doc = """The moniker to run the component under.

            Instances of `{COMPONENT_NAME}` are replaced with the component name.

            Use only for non-test, non-driver, and non-session components.
            """,
        ),
        "compiled_manifest": attr.label(
            doc = "The `fuchsia_component_manifest` target.",
            providers = [FuchsiaComponentManifestInfo],
            mandatory = True,
        ),
        "component_name": attr.string(
            doc = """The name of the component.

            This value will override any component name values that were
            set on the component manifest.
            Defaults to the component manifest file's basename.
            """,
        ),
        "is_driver": attr.bool(
            doc = """True if this is a driver component.

            Controls how the SDK runs the component.
            """,
            mandatory = True,
        ),
        "is_test": attr.bool(
            doc = """True if this is a test component.

            Controls how the SDK runs the component.
            This is independent of the `testonly` attribute.
            """,
            mandatory = True,
        ),
    },
)
