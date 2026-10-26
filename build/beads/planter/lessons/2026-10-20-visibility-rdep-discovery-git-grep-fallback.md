# Visibility rdep discovery git grep fallback

- **Learned from:** inner loop friction on a local task (task `pkg-system-image`)
- **Date:** 2026-10-20
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

checks/visibility_audit.sh invoked rg to discover reverse-dependency build files
and silently swallowed FileNotFoundError/CalledProcessError in an empty except
block without falling back to git grep across the checkout and sub-repositories,
unlike tools/find_rdeps.sh and checks/build_verification.sh. When rg was absent
from PATH, visibility_audit.sh only inspected ancestor BUILD.gn files and missed
all sibling/cross-tree callers, emitting false_positive_rdep_visibility and
overbroad_subpackages_visibility against the visibility recommended by
find_rdeps.sh.

## Why not a one-off fix

Exempting a single package or target visibility list would leave
visibility_audit.sh blind to non-ancestor reverse dependencies across every
migration task run in environments without ripgrep or with sub-repository
callers. Adding a shared git-repository discovery and git grep fallback to
visibility_audit.sh and aligning its template/sub-target rdep logic with
find_rdeps.sh fixes reverse-dependency discovery for all packages.
