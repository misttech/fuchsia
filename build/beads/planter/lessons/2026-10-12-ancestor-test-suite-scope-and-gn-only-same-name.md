# Ancestor test suite scope and gn only same name rdeps

- **Learned from:** inner loop friction on CL I14ba89b45da59f24ed2cfc6411da25aebe7dae4f, patchset 2 (task `zedmon`)
- **Date:** 2026-10-12
- **Changed:** `checks/bazel_minimality.sh`, `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

First, bazel_minimality.sh only allowed BUILD.gn and BUILD.bazel edits in the
immediate parent directory (pkg_dir == os.path.dirname(td)) rather than any
ancestor directory (td.startswith(pkg_dir + '/')), blocking test_suite() wiring
in multi-level ancestor BUILD.bazel files that commit_message_format.sh
requires. Second, visibility_audit.sh and find_rdeps.sh only treated a
hand-written GN target above BAZEL2GN SENTINEL as GN-only when its name was
absent from BUILD.bazel, so a GN group or bridge target sharing a name with a
Bazel test_suite or test target caused external GN callers of the GN group to be
misclassified as Bazel reverse dependencies requiring Bazel visibility.

## Why not a one-off fix

Hardcoding an allowlist entry for a single top-level BUILD.bazel file or a
single test_suite target name would fail whenever a nested package two or more
levels deep wires its migrated tests into an ancestor BUILD.bazel test_suite, or
whenever any package defines a hand-written GN group, bazel_test_suite, or
bazel_host_tool sharing a name with a Bazel target.
