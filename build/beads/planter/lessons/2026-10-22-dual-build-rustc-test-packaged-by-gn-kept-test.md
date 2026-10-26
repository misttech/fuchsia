# Dual build rustc test packaged by gn kept test package

- **Learned from:** inner loop friction on a local task (task `src_sys_time_lib_httpdate_hyper`)
- **Date:** 2026-10-22
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

In migration_sanity.sh, gn_kept_pkg_test_deps already identified local test
targets packaged by a GN fuchsia_unittest_package or fuchsia_test_package kept
above the BAZEL2GN SENTINEL due to GN-only blockers (such as unmigrated GN-only
component dependencies, test_type, or conditional test_specs), but that set was
discarded before the second loop where rust_unit_test_layout unconditionally
required every non-host-only rustc_test in BUILD.bazel to be accompanied by an
fx_unittest_package or fx_test in the same BUILD.bazel. Similarly,
cq_reachability.sh checked only BUILD.bazel for packaging of rustc_test targets.

## Why not a one-off fix

Exempting a single package directory would leave every other dual-build Rust
library whose unit test binary is migrated to BUILD.bazel and generated below
the BAZEL2GN SENTINEL, while its enclosing fuchsia_unittest_package or
fuchsia_test_package must stay above the sentinel due to GN-only blockers,
trapped between cq_reachability/missing_migrated_tests (when rustc_test is
omitted) and migration_sanity/rust_unit_test_layout (when rustc_test is defined
without a Bazel test package).
