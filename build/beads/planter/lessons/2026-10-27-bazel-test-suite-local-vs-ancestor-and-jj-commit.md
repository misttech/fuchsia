# Bazel test suite local vs ancestor and jj commit msg check

- **Learned from:** human review on CL I47f520f73c99d58f1d50ef3b4ce6b630401bc506, patchset 2 (task `netstack3-echo`)
- **Date:** 2026-10-27
- **Changed:** `checks/commit_message_format.sh`, `checks/cq_reachability.sh`

## Root cause

First, in checks/cq_reachability.sh, has_sibling_local_bazel_test_suite only
inspected direct children of os.path.dirname(td) rather than walking
intermediate subsystem directories between td and anc_pkg, so a package in a
nested subdirectory whose cousin packages under an intermediate parent use
per-package bazel_test_suite in BUILD.gn could still be directed to an unrelated
top-level ancestor test_suite. Second, in checks/commit_message_format.sh, the
jj colocated working-copy commit (@) fallback checked `base == "@-"` literally
instead of also matching when PLANTER_CHANGE_BASE was already resolved to @-'s
commit SHA, causing commit_message_format to skip validating @'s commit message
when git HEAD equaled PLANTER_CHANGE_BASE.

## Why not a one-off fix

Fixing only a single package's BUILD.gn/BUILD.bazel leaves nested subsystem
packages vulnerable to false ancestor test_suite routing whenever cousin
packages use local bazel_test_suite targets, and leaves jj colocated checkouts
skipping commit_message_format whenever PLANTER_CHANGE_BASE is passed as a
resolved commit SHA.
