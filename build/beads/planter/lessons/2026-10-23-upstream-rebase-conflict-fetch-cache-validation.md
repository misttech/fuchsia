# Upstream rebase conflict fetch cache validation

- **Learned from:** cq failure on CL Iba38f5ca324168715993fd4672573507be2847aa, patchset 4 (task `src_sys_time_lib_push_source`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`

## Root cause

maybe_refresh_origin_main in checks/cq_reachability.sh used a 30-second timeout
on git fetch (which times out when fetch.unpacklimit unpacks hours of upstream
commits) and unconditionally wrote the fetch cache stamp file even when git
fetch failed or timed out, poisoning the freshness cache for 600 seconds while
local refs/remotes/origin/main remained stuck at HEAD~1 and masking upstream
merge conflicts with origin/main.

## Why not a one-off fix

Manually rebasing only the current migration branch does not prevent future
migration runs in checkouts with stale origin/main refs from timing out during
git fetch, poisoning the stamp cache, and failing to detect upstream merge
conflicts before Gerrit CQ patch application.
