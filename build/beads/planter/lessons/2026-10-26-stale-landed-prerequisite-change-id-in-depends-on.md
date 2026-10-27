# Stale landed prerequisite change id in depends on

- **Learned from:** cq failure on CL Ib7799fbf9e2961e87bb3740435eed930c8b89743, patchset 6 (task `src-developer-ffx-lib-target`)
- **Date:** 2026-10-26
- **Changed:** `checks/cq_reachability.sh`

## Root cause

When prerequisite migration CLs listed by Change-Id (`I<40-hex>`) in a commit's
`Depends on:` block land on `origin/main` after an earlier patchset was uploaded
to Gerrit, Gerrit CQ tests the earlier patchset against an older base commit
that lacks the newly landed prerequisite `BUILD.bazel` package until a refreshed
commit is uploaded. `check_landed_ancestor_commits` in
`checks/cq_reachability.sh` previously only scanned `Depends on:` for numeric
`fxrev.dev/<num>` URLs (`dep_cl_re`), missing `Change-Id` (`I<40-hex>`) entries
in `Depends on:` bullets that have already landed on `origin/main`, and
unconditionally told the coder to rebase `HEAD` directly onto `origin/main` even
when other `Depends on:` prerequisite commits remained unmerged.

## Why not a one-off fix

Amending or rebasing only a single migration CL when its prerequisite CLs merge
into `origin/main` leaves every future multi-dependency migration task with
`Change-Id`-based `Depends on:` bullets undetected by `cq_reachability.sh`.
Extending `check_landed_ancestor_commits` in `checks/cq_reachability.sh` to
resolve `I<40-hex>` Change-Ids in `Depends on:` blocks against `origin/main` and
distinguish landed entries from still-unmerged prerequisite commits ensures all
stacked and multi-dependency migrations prune landed `Depends on:` entries and
upload a refreshed patchset.
