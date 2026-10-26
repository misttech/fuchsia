# Unmerged stacked parent bazel package dependency

- **Learned from:** cq failure on CL Iba38f5ca324168715993fd4672573507be2847aa, patchset 2 (task `src_sys_time_lib_push_source`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`

## Root cause

When a migrated package depends on a workspace Bazel package or target that was
migrated only in an unmerged local ancestor commit between
merge-base(origin/main, HEAD) and HEAD~1 rather than in origin/main or the
uploaded top commit HEAD, local builds succeed against the working tree while
Gerrit CQ tests the uploaded commit against origin/main and fails during Bazel
cquery (tests.json generation) with 'no such package' or missing target errors.

## Why not a one-off fix

Fixing only a single package leaves every future stacked migration task
vulnerable whenever an upstream dependency package or target was added only in
an unmerged local parent commit; deterministically verifying in
cq_reachability.sh that non-visibility workspace Bazel package and target
dependencies exist in origin/main or the top commit HEAD catches this across all
migrations before upload.
