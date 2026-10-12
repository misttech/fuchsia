# Bazel test suite platform expansion and fx test footer validation

- **Learned from:** inner loop friction on CL I5e6909ad02268b0e3db900ff8d5d521b1779fa8d, patchset 1 (task `json_merge`)
- **Date:** 2026-10-10
- **Changed:** `checks/build_verification.sh`, `checks/manifest.json`

## Root cause

When a migration wires a host test into an ancestor BUILD.bazel test_suite(),
build_verification included that test_suite-only ancestor package in
bazel_build_fuchsia (--config=fuchsia_platform) without --noexpand_test_suites,
causing Bazel to expand transitive host_py_test targets that lack a Fuchsia
Python toolchain. Additionally, build_verification did not validate Test: fx
test footers against <build dir>/tests.json during static checks.

## Why not a one-off fix

Hardcoding exclusions for a single tool directory or ancestor test_suite would
fail on every future host tool or library migration that wires migrated tests
into an ancestor BUILD.bazel test_suite() or records an unconfigured fx test
command in the commit message.
