# Unrolled unittest package bazel2gn template parity

- **Learned from:** inner loop friction on a local task (task `b2-cam2`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_target_type_parity.sh`, `checks/manifest.json`

## Root cause

When a GN fuchsia_unittest_package or fuchsia_unittest_component requires a
custom .cml manifest, test_type, or multiple unit tests that fx_unittest_package
and fx_unittest_component in Bazel (or fxUnittestPackageToGN in bazel2gn) cannot
express, it must be authored in BUILD.bazel using the unrolled
fx_component_manifest + fx_test_component + fx_package(test_components = [...])
+ fx_test chain, which bazel2gn translates to fuchsia_test_package and
fuchsia_test_component below the sentinel (matching the internal expansion of
fuchsia_unittest_package.gni and fuchsia_unittest_component.gni).
gn_target_type_parity.sh treated this template expansion as an error and did not
emit the matching INFO template_parity coverage to deduplicate review_diff.

## Why not a one-off fix

Any dual-built package whose GN fuchsia_unittest_package or
fuchsia_unittest_component specifies a custom .cml manifest, test_type, or
multiple test binaries will hit the same gn_target_type_changed and review_diff
template_parity false positives when synced through bazel2gn unless
gn_target_type_parity.sh systemically recognizes the fuchsia_unittest_package ->
fuchsia_test_package and fuchsia_unittest_component -> fuchsia_test_component
expansions.
