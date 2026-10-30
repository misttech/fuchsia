# Gn test package and bazel test suite dependency parity

- **Learned from:** human review on CL Ib0826acc04544ef980e8ffb3aff17f97df0f2137, patchset 1 (task `pkg-package-directory-tests`)
- **Date:** 2026-10-29
- **Changed:** `checks/gn_dep_parity.sh`, `checks/manifest.json`

## Root cause

gn_dep_parity treated a target name present in BUILD.bazel (such as fx_test(name
= '<test>')) as still existing in BUILD.gn and treated bazel_test_suite and
migrated fuchsia_test_package('<test>-package') targets in group('tests') as
sibling library substitutions for the original GN test package.

## Why not a one-off fix

Every dual-build package with a custom-manifest test unrolls fx_package(name =
'<test>-package', package_name = '<test>', test_components = [...]) +
fx_test(name = '<test>') and updates group('tests') in BUILD.gn to depend on
:<test>-package and :bazel_tests, so gn_dep_parity must handle migrated test
targets and bazel_test_suite edges across all packages.
