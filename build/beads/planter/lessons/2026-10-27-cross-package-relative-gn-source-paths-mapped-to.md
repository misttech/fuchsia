# Cross-package relative GN source paths (`../...`) mapped to canonical Bazel `//<owner pkg>:<rel path>` labels and post-commit base revision resolution in `gn attr parity`

- **Learned from:** inner loop friction on a local task (task `src-power-cpu-manager-energy-model-config-validator`)
- **Date:** 2026-10-27
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

In GN, targets (such as `host_test_data`, `resource`, or library/binary rules)
can reference source or input files in parent or sibling directories via `../`
relative paths across package boundaries. Bazel forbids `..` relative paths
across package boundaries, requiring those files to be exported in the owning
package's `BUILD.bazel` and referenced via canonical `//<owner_pkg>:<rel_path>`
labels. `review_diff` (`diffengine`) compares raw source strings without
normalizing workspace-relative paths across `pkg` and
`//<owner_pkg>:<rel_path>`, flagging false-positive `missing_sources` errors,
and `gn_attr_parity.sh` only emitted `missing_sources` INFO coverage for
`compile_data`/`inputs` and did not fall back to `HEAD~1` when
`PLANTER_CHANGE_BASE` resolved to an already-committed `[bazel_migration]`
`HEAD`.

## Why not a one-off fix

Any Fuchsia GN target migrating to Bazel that references parent-directory or
cross-package fixtures, test configs, headers, or sources via `../` paths must
express them as `//<owner_pkg>:<rel_path>` labels in `BUILD.bazel`. Resolving
both GN relative paths and Bazel package labels to normalized workspace-relative
paths in `gn_attr_parity.sh` and `run_checks.sh` (and ensuring
`gn_attr_parity.sh` resolves pre-migration `HEAD~1` `BUILD.gn` state when `HEAD`
is the migration commit) prevents false-positive `missing_sources` and
`unit_test_parity` failures across all packages.
