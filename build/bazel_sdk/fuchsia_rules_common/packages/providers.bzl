# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

FuchsiaPackageInfo = provider(
    doc = "Contains information about a Fuchsia package.",
    fields = {
        "fuchsia_cpu": "The target CPU specified when building this package in Fuchsia format (x64, arm64, riscv64)",
        "package_manifest": "JSON package manifest file representing the Fuchsia package.",
        "package_name": "The name of the package",
        "far_file": "The far archive",
        "meta_far": "The meta.far file",
        "files": "All files that compose this package, including the manifest and meta.far",
        "build_id_dirs": "Directories containing the debug symbols",
        "packaged_components": "A list of all the components in the form of FuchsiaPackagedComponentInfo structs",
        "package_resources": "A list of resources added to this package",
    },
)

FuchsiaComponentInfo = provider(
    doc = "Contains information about a fuchsia component",
    fields = {
        "name": "name of the component",
        "manifest": "A file representing the compiled component manifest file",
        "resources": "any additional resources the component needs",
        "moniker": "The moniker to run the non-driver, non-test, non-session component in",
        "is_driver": "True if this is a driver",
        "is_test": "True if this is a test component",
        "run_tag": "A tag used to identify the component when put in a package to be later used by the run command",
    },
)

FuchsiaPackagedComponentInfo = provider(
    doc = "Contains information about a fuchsia component that has been included in a package",
    fields = {
        "component_info": "The original FuchsiaComponentInfo provider if this is built locally. Otherwise it will be empty",
        "dest": "The install location for this component in a package (meta/foo.cm)",
    },
)

FuchsiaPackageResourcesInfo = provider(
    doc = "Contains a collection of resources to include in a package",
    fields = {
        "resources": "A list of structs containing the src and dest of the resource",
    },
)

FuchsiaCollectedPackageResourcesInfo = provider(
    doc = """A provider which represents a package resource and all of its transitive resources.

    This provider should not be directly created. If a rule wants to expose a set
    of resources it should create a FuchsiaPackageResourcesInfo provider instead.
    """,
    fields = {
        "collected_resources": "A depset containing the direct and transitive resources",
    },
)

FuchsiaDriverToolInfo = provider(
    doc = "A provider which contains information about a driver tool.",
    fields = {
        "tool_path": "A tool's binary package-relative path (e.g. 'bin/tool').",
    },
)

FuchsiaStructuredConfigInfo = provider(
    doc = "A provider which contains the generated cvf for structured configs.",
    fields = {
        "cvf_source": "The generated cvf",
        "cvf_dest": "The location where the cvf is stored within a fuchsia package archive.",
    },
)
