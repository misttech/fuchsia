# GN template parity for bazel2gn-generated test executables (cc test executable vs executable)

- **Learned from:** inner loop friction on a local task (task `virt-libs`)
- **Date:** 2026-10-27
- **Changed:** `checks/gn_target_type_parity.sh`, `checks/manifest.json`

## Root cause

gn_target_type_parity required every converted GN target to keep its exact
template, but bazel2gn maps fx_cc_binary/cc_binary only to executable() and
cannot emit cc_test_executable(); meanwhile the dual-build test rules forbid
skipping the test binary, so the check contradicted the prescribed migration
path for every C++ test that used cc_test_executable.

## Why not a one-off fix

Any package whose C++ unit test binary is a GN cc_test_executable hits the same
contradiction once its tests move to Bazel; waiving the finding for guest_config
alone would leave every future C++ test migration stuck disputing the same
unfixable ERROR.
