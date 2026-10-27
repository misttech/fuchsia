# Unpackaged bazel rust test with gn only test package and relative gn rdeps

- **Learned from:** human review on CL I714afb1d66bf041eca70dc0b7af8841907fba02f, patchset 2 (task `src-power-manager-configs-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`, `checks/migration_sanity.sh`

## Root cause

First, checks/migration_sanity.sh (in is_component_or_gn_packaged_rust_test) and
checks/cq_reachability.sh (via is_packaged_by_kept_gn_test_package) exempted any
bare rustc_test() in BUILD.bazel from unit-test packaging and reachability
checks whenever a kept GN-only fuchsia_unittest_package or fuchsia_test_package
above the BAZEL2GN SENTINEL in BUILD.gn depended on the generated test target.
When a test package must remain GN-only in BUILD.gn (for example, because it
tests outputs of a GN-only template), migrating only the inner rustc_test() to
BUILD.bazel without a Bazel test package rule (fx_unittest_package or fx_test)
leaves a dead, unpackaged, unrunnable device test binary in the Bazel graph that
nothing in Bazel depends on. Second, relative GN dependency labels using parent
directory traversal (../) in sibling or cousin packages were not matched when
scanning reverse dependencies.

## Why not a one-off fix

Reverting only the specific template test directories in this task would leave
the exemption in checks/migration_sanity.sh and checks/cq_reachability.sh
intact, allowing future migrations of directories with GN-only test packages to
continue producing bare unpackaged rustc_test() targets in BUILD.bazel. Removing
the kept-GN-package exemption in both checks/migration_sanity.sh and
checks/cq_reachability.sh ensures that any bare device rustc_test() in
BUILD.bazel without a Bazel test package or component wrapper is
deterministically flagged across all future migrations.
