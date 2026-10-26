# Unmerged stacked bazel package dependency in cq

- **Learned from:** cq failure on CL Ib40c2fc170fe74610b16ba47ed02d1d99d286440, patchset 2 (task `src_sys_time_lib_inspect_writable`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

In checks/cq_reachability.sh, check_unmerged_parent_bazel_package_deps() checked
is_ancestor(origin_main, parent_sha) after maybe_refresh_origin_main() fetched
the latest origin/main. Whenever upstream origin/main advanced by even one
commit after the local stacked branch was created, origin/main was no longer a
direct ancestor of HEAD~1, causing check_unmerged_parent_bazel_package_deps() to
return early without flagging BUILD.bazel dependencies that only existed in
unmerged local ancestor commits (merge-base..HEAD~1). In addition, recommending
git reset --soft origin/main when origin/main had advanced past the branch
merge-base risked reverting upstream commits during the soft reset.

## Why not a one-off fix

Fixing only a single migrated package or manually squashing its dependency
commit does not prevent future stacked GN-to-Bazel migrations from bypassing
unmerged_parent_bazel_package_dep whenever origin/main advances during task
execution. Computing stack_base via git merge-base origin/main HEAD, scanning
all top-commit BUILD.bazel files while ignoring visibility-only package specs
(:__pkg__, :__subpackages__), and folding against $(git merge-base origin/main
HEAD) ensures every stacked migration detects unmerged parent Bazel package
dependencies before CQ upload.
