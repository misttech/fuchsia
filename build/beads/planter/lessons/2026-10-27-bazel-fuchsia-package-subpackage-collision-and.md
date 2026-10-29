# Bazel fuchsia package subpackage collision and global build allowlist exclusion

- **Learned from:** inner loop friction on CL Ie9d651577c24f541db3b76fe7c4d99578fa4d076, patchset 4 (task `fake-netstack`)
- **Date:** 2026-10-27
- **Changed:** `checks/build_verification.sh`, `checks/gn_dep_parity.sh`, `checks/gn_target_type_parity.sh`, `checks/migration_sanity.sh`

## Root cause

First, check_cross_tree_package_name_collisions in migration_sanity.sh checked
all fx_package targets rather than scoping collision checks to test packages
(is_test_pkg) published to amber-files/repository via bazel_test_suite, falsely
flagging non-test drop-in replacement subpackages that intentionally share a
runtime package_name with other packages to satisfy fixed .cml subpackage URLs.
Second, build_verification.sh (and sibling parity checks) excluded top-level
aggregator directories ("tools", "src", "sdk") when collecting modified BUILD
files, but omitted top-level "build", so editing build/BUILD.gn to register
bazel_fuchsia_package visibility in group("non_hermetic_deps") caused
build_verification to treat //build as a migrated package and execute
//build:host_tests (//build:shac_test) on RBE where shac's internal nsjail
fails.

## Why not a one-off fix

Hardcoding an exception for a single benchmark package directory would fail
whenever any other drop-in subpackage or non-test fx_package wrapped with
bazel_fuchsia_package updates group("non_hermetic_deps") in build/BUILD.gn or
shares a subpackage package_name across the tree.
