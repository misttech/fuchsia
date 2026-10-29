# Ancestor test suite subtree coverage and attr parity helper definition

- **Learned from:** inner loop friction on a local task (task `b4-exd1`)
- **Date:** 2026-10-29
- **Changed:** `checks/build_only_scope.sh`, `checks/cq_reachability.sh`, `checks/gn_attr_parity.sh`

## Root cause

gn_attr_parity.sh called find_planter_bin() in
collect_diffengine_alignment_findings() without defining the helper function,
and build_only_scope.sh referenced undefined target_dir instead of raw_dirs when
PLANTER_TARGET_DIRS was unset, causing NameError crashes before printing JSON
findings (which also prevented gn_attr_parity.sh INFO dep_alignment findings for
GN forwarding groups and merged test_deps from deduplicating review_diff
warnings). Additionally, ancestor_suite_covers_subtree() in cq_reachability.sh
required an existing test_suite member in the exact first-level child directory
under anc_pkg even for flat area test suites that directly aggregate leaf tests
across all subdirectories.

## Why not a one-off fix

Fixing only a single package directory would leave gn_attr_parity.sh crashing on
every migration task and would continue rejecting valid test_suite registrations
whenever a package is the first migrated test in its immediate subdirectory
under an existing flat area test_suite.
