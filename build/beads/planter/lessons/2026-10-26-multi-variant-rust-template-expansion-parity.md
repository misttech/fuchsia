# Multi variant rust template expansion parity

- **Learned from:** inner loop friction on a local task (task `netstack3 datagram`)
- **Date:** 2026-10-26
- **Changed:** `checks/gn_attr_parity.sh`, `checks/gn_target_type_parity.sh`, `tools/run_checks.sh`

## Root cause

While gn_target_type_parity already recognized expanding a GN
rustc_library_with_features macro into explicit Bazel rustc_library targets for
the base library and each feature_sets variant, and gn_attr_parity recorded INFO
coverage for the attribute_alignment rule mapping notice, neither check emitted
the matching INFO template_parity finding (nor did run_checks downgrade it),
leaving review_diff's built-in template_parity check un-deduplicated as a
false-positive ERROR.

## Why not a one-off fix

Any Fuchsia Rust crate using the GN-only rustc_library_with_features wrapper
macro must expand its feature_sets variants into explicit rustc_library targets
in BUILD.bazel because bazel2gn has no multi-variant wrapper rule mapping;
recording INFO template_parity coverage in gn_target_type_parity and
gn_attr_parity (and downgrading it in run_checks) resolves this across all
multi-variant Rust crate migrations.
