# Stacked prerequisite chain and bazel dep package existence

- **Learned from:** cq failure on CL Ib7799fbf9e2961e87bb3740435eed930c8b89743, patchset 5 (task `src-developer-ffx-lib-target`)
- **Date:** 2026-10-26
- **Changed:** `checks/cq_reachability.sh`

## Root cause

The `check_landed_ancestor_commits` check in `checks/cq_reachability.sh` only
scanned `Depends on:` commit-message entries for `fxrev.dev/<number>` URLs and
ignored Gerrit `Change-Id`s (`I[0-9a-f]{40}`), and its remediation instructed
the agent to rebase `HEAD` directly onto `origin/main` without warning against
`git rebase --onto origin/main HEAD~1` when unlanded prerequisite commits remain
in the stack. Dropping unlanded prerequisite commits from the local parent stack
(or depending on an in-tree Bazel package whose `BUILD.bazel` is absent)
detaches the Gerrit CL from its unlanded prerequisites and causes CQ `fx gen`
(`bazel query` for `tests.json`) to fail with `no such package: BUILD file not
found`.

## Why not a one-off fix

Fixing only the single target directory's commit stack or `Depends on:` block
leaves future multi-CL stacked migrations vulnerable to dropping unlanded
prerequisite ancestor commits during rebases, retaining stale landed `Change-Id`
`Depends on:` entries, or referencing in-tree Bazel dependency packages whose
`BUILD.bazel` is missing from the checkout.
