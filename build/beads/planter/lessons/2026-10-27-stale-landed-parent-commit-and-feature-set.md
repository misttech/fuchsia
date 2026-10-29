# Stale landed parent commit and feature set template parity

- **Learned from:** inner loop friction on a local task (task `netstack3-echo`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`, `checks/gn_attr_parity.sh`

## Root cause

check_landed_ancestor_commits in cq_reachability.sh scanned the entire HEAD
commit message for any Gerrit CL URL even when HEAD was still at
$PLANTER_CHANGE_BASE (an ancestor of origin/main with uncommitted working-tree
changes), matching HEAD's own 'Reviewed-on:' footer after origin/main advanced.
In addition, collect_diffengine_alignment_findings in gn_attr_parity.sh only
searched PATH and ~/.local/bin/planter without checking /proc/<pid>/exe for the
running planter daemon when collecting template_parity and attribute_alignment
INFO suppressions.

## Why not a one-off fix

Any migration task where origin/main advances while the coding agent has
uncommitted working-tree edits on top of an upstream base commit (or where a
commit message contains a 'Reviewed-on:' footer) would hit the same
false-positive stale_landed_parent_commit error, and any
rustc_library_with_features migration run under a daemon binary not on PATH
would miss diffengine alignment reconciliation unless fixed in the shared check
scripts.
