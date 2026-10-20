# Dual build unit test package and cc binary alignment parity

- **Learned from:** inner loop friction on a local task (task `sockscripter`)
- **Date:** 2026-10-16
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

First, migration_sanity.sh included fx_unittest_package (and testonly fx_package
targets) in fx_pkgs when running non-test package checks, causing
stale_assembly_bazel_inputs_bridge to falsely flag assembly bridges for
unmigrated componentless shell-tool fuchsia_package targets whenever a unit test
was migrated to fx_unittest_package. Second, gn_attr_parity.sh and run_checks.sh
did not record INFO alignment coverage for C/C++ binaries preserving GN
output_name via @bazel2gn:raw_overwrite (since fx_cc_binary/cc_binary has no
output_name attribute) or for third_party_target_map.json C++ dependency
mappings such as @com_google_googletest//:gtest covering both
//third_party/googletest:gtest and //third_party/googletest:gmock.

## Why not a one-off fix

Any dual-build package that migrates C++ unit tests to fx_unittest_package while
retaining a componentless shell-tool fuchsia_package in BUILD.gn, sets
output_name on a C++ executable whose name collides with a group(), or depends
on mapped third-party C++ libraries like googletest/gmock will hit these same
false-positive migration_sanity and review_diff findings unless handled
systemically in the check scripts.
