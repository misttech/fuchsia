# Bazel test suite wildcard expansion on target platform

- **Learned from:** inner loop friction on CL I5e6909ad02268b0e3db900ff8d5d521b1779fa8d, patchset 1 (task `json_merge`)
- **Date:** 2026-10-10
- **Changed:** `checks/build_verification.sh`

## Root cause

When a migration wires a host test into an ancestor or package test_suite() with
target_compatible_with = HOST_CONSTRAINTS, running fx bazel build
--config=fuchsia_platform //<dir>:all without --noexpand_test_suites causes
Bazel to expand test_suite() targets during wildcard target-pattern evaluation
before checking target_compatible_with, pulling in transitive host_py_test
targets whose underlying py_binary rules lack target_compatible_with and fail
Python toolchain resolution on fuchsia_platform.

## Why not a one-off fix

Hardcoding an exclusion for a single ancestor directory in build_verification.sh
would still fail whenever any other package or ancestor aggregator BUILD.bazel
defining a host-constrained test_suite() or host_py_test is built with :all
under --config=fuchsia_platform.
