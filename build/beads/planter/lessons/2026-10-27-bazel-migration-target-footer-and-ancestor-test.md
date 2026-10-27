# Bazel migration target footer and ancestor test suite aggregation

- **Learned from:** human review on CL I9492e89e2c7d9dbc88feba65ec6760313bdd095c, patchset 3 (task `src_sys_time_httpsdate_time_source`)
- **Date:** 2026-10-27
- **Changed:** `checks/commit_message_format.sh`, `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

First, in checks/commit_message_format.sh, the regex `^([A-Za-z_]\w*)\((.*?)^\)`
with `re.M | re.S` required a closing parenthesis at column 0 (`^\)`), so any
single-line `package(default_applicable_licenses = [...])` declaration at the
top of a `BUILD.bazel` file swallowed the entire following multi-line target
block (such as `rustc_binary(name = "bin", ...)`), misclassifying that target
under `rule = "package"` (`helper_rules`), rejecting its
`Bazel-Migration-Target:` footer as `bazel_migration_target_not_migrated`, and
never checking for omitted migrated GN targets when other
`Bazel-Migration-Target:` lines were present. Second, checks/cq_reachability.sh
accepted newly added local `bazel_test_suite` targets in a migrated package's
`BUILD.gn` even when an ancestor `BUILD.bazel` already defined an aggregate
`test_suite` (`target_tests` or `host_tests`) exported via that ancestor's
`BUILD.gn`, and its `unexported_bazel_test` remediation instructed adding a
local `bazel_test_suite` instead of wiring the test into the ancestor
`test_suite`.

## Why not a one-off fix

Fixing only the single package's commit message and BUILD.gn leaves
`commit_message_format.sh` swallowing the first target after any single-line
`package(...)` call across all migrated `BUILD.bazel` files and leaves
`cq_reachability.sh` permitting redundant per-package `bazel_test_suite` bridges
wherever an ancestor subsystem `BUILD.bazel` already aggregates `target_tests`
or `host_tests`.
