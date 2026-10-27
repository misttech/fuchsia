# Subrepository upstream origin main rdep visibility discovery

- **Learned from:** cq failure on CL Id6859310277b143b530192d56d8d0d86decb5cb8, patchset 1 (task `src-power-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`, `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

When the root checkout was fetched and rebased onto upstream origin/main across
multi-day worktree sessions, nested sub-repositories remained pinned at their
older local working-tree snapshot and their origin/main refs were never
refreshed or searched by find_rdeps.sh and visibility_audit.sh. As a result,
callers newly landed on upstream origin/main in a sub-repository were invisible
to local reverse-dependency discovery and visibility auditing, causing CQ
builders to fail with GN visibility 'Dependency not allowed' errors.

## Why not a one-off fix

Manually widening visibility in a single package's BUILD.bazel would leave every
future migration vulnerable whenever a sub-repository's local working tree lags
behind upstream origin/main. Refreshing origin/main across all discovered git
repositories and grepping both working-tree and origin/main build files in
find_rdeps.sh and visibility_audit.sh systemically catches upstream callers
across all packages.
