# Unmerged parent bazel package dep in standalone gerrit upload

- **Learned from:** cq failure on CL Ib40c2fc170fe74610b16ba47ed02d1d99d286440, patchset 1 (task `src_sys_time_lib_inspect_writable`)
- **Date:** 2026-10-22
- **Changed:** `checks/cq_reachability.sh`

## Root cause

When a migration task is stacked locally on top of a cherry-picked unmerged
prerequisite commit (origin/main..HEAD~1), the prerequisite package's
BUILD.bazel exists on disk in the local worktree so local Bazel queries and
builds succeed. However, when the task's top commit (HEAD~1..HEAD) is uploaded
to Gerrit parented on origin/main, the prerequisite package's BUILD.bazel is
missing from the uploaded CL tree on origin/main, causing Gerrit CQ (tricium
tests.json cquery) to fail with 'no such package: BUILD file not found'.
Existing checks only verified mergeability against origin/main and did not
verify that referenced workspace Bazel packages exist in origin/main or the top
commit.

## Why not a one-off fix

Any migration task whose BUILD.bazel depends on a library or proc-macro package
migrated in an unmerged prerequisite commit will pass local worktree checks
while failing Gerrit CQ cquery when uploaded against origin/main unless
cq_reachability deterministically verifies that every referenced workspace Bazel
package exists either in origin/main or in the top commit HEAD~1..HEAD.
