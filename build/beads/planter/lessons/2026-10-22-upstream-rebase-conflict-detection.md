# Upstream rebase conflict detection

- **Learned from:** cq failure on CL I0ead572c4a5deb29da8937074f99a45810686959, patchset 2 (task `src_sys_time_testing_fake_hrtimer`)
- **Date:** 2026-10-22
- **Changed:** `checks/cq_reachability.sh`

## Root cause

The existing cq_reachability check used a 600-second fetch TTL for refreshing
origin/main and only staged untracked files under target_dirs when constructing
the synthetic worktree tree for git merge-tree, allowing recently landed
upstream commits on shared dependency visibility lists or newly added build
files outside target_dirs to go undetected between rounds.

## Why not a one-off fix

Rebasing only a single package CL does not prevent future migration CLs that
touch shared dependency visibility lists or sibling packages from encountering
stale origin/main refs within a 10-minute window or missing untracked build
files during pre-submit merge-tree checks.
