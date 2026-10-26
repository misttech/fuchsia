# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Allowlists of restricted CML features for Bazel `fx_component_manifest()`.

These mirror the GN `group()` allowlists in
//tools/cmc/build/restricted_features/BUILD.gn, which GN enforces through the
`visibility` of the group that `cm()` depends on for each requested feature.
Bazel can't enforce target-level visibility (Bazel visibility is per package),
so `fx_component_manifest()` instead matches its own label against these
lists, which use the same GN visibility syntax as BUILD.gn:

*   `"*"`: any target (a GN group without `visibility`).
*   `"//foo/bar:*"`: any target in `//foo/bar`.
*   `"//foo/bar/*"`: any target in `//foo/bar` or a subdirectory.
*   `"//foo/bar:baz"`: only `//foo/bar:baz`.

Features that do not have a Bazel user yet map to `None`.
"""

visibility(["//build/bazel/rules/components/..."])

RESTRICTED_FEATURE_ALLOWLISTS = {
    # Available to all components in fuchsia.git.
    "structured_config": ["*"],
    # LINT.IfChange(dynamic_dictionaries)
    "dynamic_dictionaries": [
        # Tests of the Bazel `fx_component_manifest(restricted_features = ...)`.
        "//build/bazel/rules/packages/tests:example_compiled_manifest",
        "//build/bazel/rules/packages/tests:example_dynamic_dictionaries_manifest",
        "//examples/components/dictionaries/*",
        "//src/bringup/bin/kernel-args-forwarder:*",
        "//src/diagnostics/archivist:*",
        "//src/diagnostics/archivist/tests/integration/realm_factory:*",
        "//src/diagnostics/persistence:*",
        "//src/diagnostics/persistence/tests:*",
        "//src/security/tests/pkg_test/*",
        "//src/sys/component_manager:dispatcher_cm",
        "//src/sys/component_manager/tests/*",
        "//src/sys/pkg/tests/pkg-cache-as-base-resolver/as-a-component-resolver/*",
    ],
    # LINT.ThenChange(//tools/cmc/build/restricted_features/BUILD.gn:dynamic_dictionaries)
    # Available to all components in fuchsia.git.
    "enable_allow_non_hermetic_packages_feature": ["*"],
    # Available to all components in fuchsia.git.
    "restrict_test_type_in_facets": ["*"],

    # GN-known restricted features that do not have a Bazel user yet.
    # When a Bazel component needs one of these, mirror its allowlist from
    # //tools/cmc/build/restricted_features/BUILD.gn (or
    # //tools/cmc/build/restricted_features/security_owned/BUILD.gn) here.
    "allow_long_names": None,
    "allow_non_hermetic_packages": None,
    "use_dictionaries": None,
}
