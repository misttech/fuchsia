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
