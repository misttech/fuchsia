# Subrepository upstream rdep visibility discovery

- **Learned from:** inner loop friction on CL Id6859310277b143b530192d56d8d0d86decb5cb8, patchset 1 (task `src-power-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

In visibility_audit.sh and find_rdeps.sh, maybe_refresh_repo_origin_main invoked
git fetch directly on sub-repositories whose remote.origin.url uses the sso://
protocol. Inside the sandbox where /var/run/ccache is mounted read-only,
git-remote-sso aborts with a read-only filesystem error when attempting to chmod
the SSO cookie directory, leaving the sub-repository's local origin/main ref at
a stale Jiri snapshot and causing visibility_audit to emit
false_positive_rdep_visibility when visibility is granted for callers added
upstream on the sub-repository's main branch.

## Why not a one-off fix

Exempting a single migrated library or hardcoding target-specific visibility
exceptions would leave every future migration whose callers were added upstream
in a lagging sso:// sub-repository checkout vulnerable to either missing
upstream reverse dependencies before upload or failing visibility_audit with
false_positive_rdep_visibility after CQ reports the caller. Rewriting sso://
remotes to https:// during origin/main refresh in visibility_audit.sh and
find_rdeps.sh and suppressing false-positive sub-repository visibility errors
when the local sub-repository origin/main ref remains stale relative to the root
checkout resolves this class of defect across all packages.
