# Cross tree fuchsia package name collision in unit test packaging

- **Learned from:** cq failure on CL I02859986d803c81f632aaa0fd276b0ec7a86cc1a, patchset 2 (task `src-lib-mdns-rust`)
- **Date:** 2026-10-23
- **Changed:** `checks/migration_sanity.sh`

## Root cause

When migrating a crate whose original BUILD.gn defined an unpackaged unit test
binary (with_unit_tests = true without a fuchsia_unittest_package), the coding
agent wraps the migrated rustc_test in a new fx_unittest_package. Existing
checks only verified same-directory duplicate test package names and did not
check whether the chosen Fuchsia package_name (or its bazel2gn -gn twin)
collides with an existing fuchsia_package, fuchsia_unittest_package,
fuchsia_test_package, fx_package, or fx_unittest_package in another directory of
the checkout, causing CQ builders that stage all universe test packages into
amber-files/repository to fail with 'conflict between package manifests'.

## Why not a one-off fix

Renaming only a single colliding test package leaves every future migration that
introduces an fx_unittest_package or fx_package vulnerable to choosing a generic
<crate>-tests package name already claimed by another subsystem in the
repository. Enforcing global Fuchsia package_name uniqueness across BUILD.gn and
BUILD.bazel in migration_sanity.sh catches cross-tree package manifest
collisions deterministically before CQ.
