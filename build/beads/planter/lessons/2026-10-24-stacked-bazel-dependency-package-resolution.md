# Stacked bazel dependency package resolution

- **Learned from:** cq failure on CL I4b835ab2d696fc7a345394547335937da73039a8, patchset 4 (task `src-developer-ffx-lib-discovery`)
- **Date:** 2026-10-24
- **Changed:** `checks/cq_reachability.sh`

## Root cause

When a migrated BUILD.bazel depends on a Bazel package provided or modified by a
stacked parent commit above origin/main, dropping the parent commit during
rebase or failing to detect stacked parent modifications to pre-existing
BUILD.bazel files causes CQ tests.json bazel query to fail with no such package,
and unclear remediation when the stack is re-cherry-picked risks duplicating
parent files into HEAD or disputing passing checks.

## Why not a one-off fix

Fixing only a single package migration leaves every future stacked migration
chain vulnerable when a parent commit modifies an existing BUILD.bazel file or
when a child commit is re-stacked after a prior CQ failure; checking git diff
between origin/main and the effective diff base in cq_reachability.sh protects
all stacked Bazel migrations.
