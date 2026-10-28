# Dual build package bridge and kept gn test packaging checks

- **Learned from:** inner loop friction on a local task (task `wlancfg-pkg`)
- **Date:** 2026-10-26
- **Changed:** `checks/gn_attr_parity.sh`, `checks/migration_sanity.sh`

## Root cause

Three static check defects caused false-positive errors and warnings on
dual-build package migrations: (1) stale_assembly_bazel_inputs_bridge in
migration_sanity.sh used substring matching ('//' + bridge_dir_rel in text),
which false-positived when an unmigrated child subpackage bridge under
'//bundles/assembly/bazel_inputs/<dir>/<sub>' remained referenced in
bundles/assembly/BUILD.bazel or bundles/assembly/bazel_inputs/BUILD.gn, and
the kept GN test package scan only looked up test components above the
BAZEL2GN SENTINEL; (2) a rust_unit_test_layout false positive on rustc_test
targets packaged by a kept GN test package, which is already fixed upstream
(check_rust_unit_test_layout) and not changed here; (3) a stale Depends on: false positive in
check_landed_ancestor_commits (cq_reachability.sh), which is also fixed
upstream and not changed here; and (4) check_rust_target_attr_alignment_info in
gn_attr_parity.sh did not emit INFO attribute_alignment coverage for 'edition in
GN not carried over to Bazel' on rustc_test(crate = ':<crate>') targets that
inherit edition from their crate target.

## Why not a one-off fix

Exempting a single package directory or hardcoding target names would leave
every other package that has unmigrated child subpackage assembly bridges,
GN-blocked test package dependencies wrapping dual-built rustc_test binaries,
uncommitted working-tree changes on top of a landed HEAD commit, or
crate-inheriting rustc_test targets vulnerable to the same false-positive check
failures.
