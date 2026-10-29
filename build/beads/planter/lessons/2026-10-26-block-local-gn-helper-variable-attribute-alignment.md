# Block local gn helper variable attribute alignment

- **Learned from:** inner loop friction on a local task (task `netstack3-device`)
- **Date:** 2026-10-26
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

In GN build files, assignments to underscore-prefixed identifiers (such as
_testutils_deps or _common_sources) inside a target invocation block are
block-local helper variables ignored by forward_variables_from rather than
target attributes. While gn_attr_parity generated composite attribute_alignment
INFO findings that included extra attributes alongside configs notes,
is_benign_alignment_clause only matched the fixed BENIGN_UNMAPPED_GN_ATTRS
allowlist and gn_attr_parity did not emit standalone attribute_alignment INFO
findings for underscore-prefixed helper variables or unmapped
rustc_library_with_features attributes when emitted individually by review_diff.

## Why not a one-off fix

Hardcoding a single variable name or target package would fail whenever another
migrated GN target defines a differently named underscore-prefixed block-local
helper variable inside its target body or expands rustc_library_with_features
with lint_config set in Bazel. Treating all underscore-prefixed GN block-local
helper variables and standalone unmapped rustc_library_with_features attributes
as benign INFO alignment findings in gn_attr_parity resolves this class of
false-positive warning across the entire repository.
