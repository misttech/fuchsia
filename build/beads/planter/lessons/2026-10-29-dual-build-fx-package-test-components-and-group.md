# Dual build fx package test components and group dep parity

- **Learned from:** inner loop friction on a local task (task `pkg-package-directory-tests`)
- **Date:** 2026-10-29
- **Changed:** `checks/gn_dep_parity.sh`, `checks/gn_target_type_parity.sh`, `checks/manifest.json`

## Root cause

When migrating custom-manifest GN device tests to dual-build Bazel
fx_package(test_components = ...) + fx_test + bazel_test_suite, two parity
checks rejected the required bazel2gn output: (1) gn_dep_parity treated the
fx_test target in BUILD.bazel as an existing target of the old GN test-package
name and flagged group("tests") replacing that GN test package with
bazel_test_suite and the renamed <package_name>-gn fuchsia_test_package as
dep_label_substituted; and (2) gn_target_type_parity only allowed
fuchsia_test_component without test_type to become testonly fuchsia_component
(missing the symmetric testonly fuchsia_component to fuchsia_test_component
conversion that fx_package(test_components) and bazel2gn require, as well as the
matching INFO template_parity finding needed to deduplicate review_diff).

## Why not a one-off fix

Every Fuchsia dual-build package whose device tests use a custom .cml manifest
(or extra packaged components/resources) must declare fx_test with the original
test name, rename its fx_package target (synced by bazel2gn as
fuchsia_test_package with a -gn package_name wired into group("tests")), and
declare test roots with fx_test_component (synced by bazel2gn as
fuchsia_test_component even when base BUILD.gn used testonly fuchsia_component).
Exempting a single directory would leave gn_dep_parity, gn_target_type_parity,
and review_diff blocking every future custom-manifest dual-build test migration.
