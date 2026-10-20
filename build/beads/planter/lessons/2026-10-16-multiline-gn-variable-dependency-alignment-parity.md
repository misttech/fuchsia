# Multiline gn variable dependency alignment parity

- **Learned from:** inner loop friction on CL Ia917b210ccfb0ba163984e259811e3cdbb419d16, patchset 2 (task `netstack3-base`)
- **Date:** 2026-10-16
- **Changed:** `checks/gn_dep_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When gn format wraps a generated BUILD.gn dependency assignment across a newline
immediately after `deps =` (such as `deps =\n _VAR1 + _VAR2 + [...]` inside `if
(is_host)` blocks generated from `target_compatible_with =
HOST_OS_CONSTRAINTS`), the single-line GN dependency parser in `review_diff`
misses the wrapped `deps =` statement and emits a false-positive `dep_alignment`
WARNING reporting all `deps` entries as unmatched in Bazel. In addition,
`gn_dep_parity.sh` only parsed literal `attr = [...]` brackets rather than
top-level GN list variables and `+` concatenations, and `migration_sanity.sh`
did not pre-empt false-positive `dep_alignment` warnings when the
multi-line-expanded GN and Bazel dependency sets match.

## Why not a one-off fix

Any dual-build package that shares dependency lists across multiple feature
variants via top-level `_VAR` variables (especially packages migrated from
`rustc_library_with_features` with host-only `loom` or `testutils` variants)
will have `gn format` wrap `deps =` onto a new line when nested inside `if
(is_host)`, triggering the same false-positive `dep_alignment` warning unless
multi-line GN variable concatenations are resolved across all checks.
