# Bazel fuchsia package gn wrapper parity

- **Learned from:** human review on CL Ie9d651577c24f541db3b76fe7c4d99578fa4d076, patchset 2 (task `fake-netstack`)
- **Date:** 2026-10-26
- **Changed:** `checks/gn_dep_parity.sh`, `checks/gn_target_type_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

Two deterministic checks prevented replacing a migrated GN fuchsia_package with
bazel_fuchsia_package: (1) migration_sanity.sh treated any fx_package with
testonly = True as a test package (is_test_pkg = True) even when it only
packaged non-test components, excluding it from fx_pkgs and skipping
duplicate_gn_fuchsia_package detection on the remaining GN fuchsia_package, and
(2) gn_target_type_parity.sh (and gn_dep_parity.sh) did not recognize
fuchsia_package / fuchsia_package_with_single_component -> bazel_fuchsia_package
as a valid GN-to-Bazel package wrapper conversion when GN dependents remain.

## Why not a one-off fix

Fixing only a single package directory leaves migration_sanity blind to all
testonly fx_package targets that duplicate a GN fuchsia_package and leaves
gn_target_type_parity and gn_dep_parity rejecting bazel_fuchsia_package wrappers
across all future dual-build package migrations.
