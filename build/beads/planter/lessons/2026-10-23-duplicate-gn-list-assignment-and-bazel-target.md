# Duplicate gn list assignment and bazel target merge collision

- **Learned from:** cq failure on CL I74f11c25ffec3364aa0bd0c1913126a3e09a4550, patchset 6 (task `src_developer_ffx_tools_speedtest_common`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

When multiple concurrent migrations add a sibling test_suite (such as
target_tests) to an ancestor BUILD.bazel and a corresponding target_tests =
[...] assignment to bazel_test_suite in the ancestor BUILD.gn (or when a rebase
merges non-overlapping insertions on adjacent lines), git 3-way merge succeeds
without textual conflict markers while producing duplicate non-empty list
assignments in the same GN scope (failing gn gen across CQ builders with
'Replacing nonempty list') and duplicate target definitions in BUILD.bazel.
Neither gn_format (--dry-run) nor cq_reachability previously audited
changed/ancestor or git-merge-tree-merged BUILD.gn and BUILD.bazel files for
duplicate non-empty GN list '=' assignments or duplicate top-level Bazel
targets.

## Why not a one-off fix

Fixing only a single ancestor BUILD.gn or BUILD.bazel leaves every other shared
ancestor test aggregator across the repository vulnerable to silent 3-way merge
collisions or duplicate attribute/target additions during parallel migrations
and rebases. Extending cq_reachability.sh to deterministically detect duplicate
non-empty GN list '=' assignments within the same scope and duplicate top-level
BUILD.bazel target names in both working-tree files and 3-way merged origin/main
trees catches this failure class across all packages before CQ.
