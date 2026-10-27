# Worktree and subrepo rdep discovery parity

- **Learned from:** inner loop friction on a local task (task `pkg-fs-management-rust`)
- **Date:** 2026-10-19
- **Changed:** `tools/find_rdeps.sh`

## Root cause

When ripgrep (rg) was unavailable or when running inside a git worktree where
.git is a regular file rather than a directory, reverse-dependency file
discovery in visibility_audit.sh and find_rdeps.sh diverged or skipped git grep
fallback and untracked build files, causing false_positive_rdep_visibility
errors on valid caller packages.

## Why not a one-off fix

Hardcoding visibility exceptions for individual packages would leave
visibility_audit.sh and find_rdeps.sh broken for all other migrations running in
git worktrees or environments without ripgrep on PATH.
