# Cq reachability uncommitted change base and depends on parsing

- **Learned from:** inner loop friction on a local task (task `src-power-lib-batch`)
- **Date:** 2026-10-26
- **Changed:** `checks/cq_reachability.sh`

## Root cause

checks/cq_reachability.sh invoked check_landed_ancestor_commits whenever
head_sha != origin_main (including when HEAD was an upstream commit on
origin/main that origin/main had advanced past after git fetch, or when
PLANTER_CHANGE_BASE was HEAD during an uncommitted working-tree migration), and
matched Gerrit CL URLs across the entire HEAD commit message (matching
Reviewed-on: footers on upstream commits) instead of scoping CL URL extraction
to explicit 'Depends on:' sections.

## Why not a one-off fix

Ignoring a single CL number or target directory would still allow
cq_reachability to inspect upstream or base commit messages whenever origin/main
advances during an uncommitted working-tree migration, or misinterpret
Reviewed-on: footers and prose CL links as stale 'Depends on:' declarations
across any future migration task.
