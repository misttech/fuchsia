# Dual build test and component parity when test package blocked

- **Learned from:** human review on CL If2ee09fc873ef597f74c3f851d001680998678d2, patchset 1 (task `pkg-dns-lib`)
- **Date:** 2026-10-20
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`
- **Superseded by:** `2026-10-22-policy-drift-public-visibility-and-test-skip.md` (test skips only)

## Status: partly superseded

The `check_unmigrated_kept_pkg_test_targets` check this lesson added was
dropped when it was rebased onto the current test policy: bazel2gn now
generates GN copies of tests (no `# @bazel2gn:skip` on tests by default), and
tests that stay GN-only for dual-testing cost are confirmed by a human through
the `# Needs reviewer attention (bazel2gn skip):` marker, which a check that
forces every Bazel-buildable kept GN test into BUILD.bazel would contradict.

## Root cause

When a GN test package (fuchsia_test_package or fuchsia_unittest_package) stayed
above the BAZEL2GN SENTINEL in BUILD.gn because one of its dependencies was
blocked on a GN-only construct, migration_sanity.sh recorded
gn_kept_pkg_test_deps and exempted all pre-sentinel rustc_test/go_test targets
as INFO (gn_test_kept_above_sentinel) without checking whether the test target
itself and its test component (fuchsia_component or fuchsia_test_component) had
only Bazel-buildable dependencies and could be dual-built in BUILD.bazel.

## Why not a one-off fix

Fixing only a single package directory leaves migration_sanity.sh blind to any
other dual-build package where a fuchsia_test_package is blocked by one GN-only
sibling dependency while its underlying rustc_test/go_test binary and
fuchsia_component/fuchsia_test_component have all-Bazel-buildable dependencies
and should be dual-built in BUILD.bazel.
