# Dual build structured config and fx manifest parity

- **Learned from:** inner loop friction on a local task (task `pkg-cache-config`)
- **Date:** 2026-10-20
- **Changed:** `checks/build_verification.sh`, `checks/gn_attr_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

First, build_verification.sh excluded <name>_validate_ir_json for direct
fidl_library calls in //dir:all builds, but missed
<name>_fidl_internal_validate_ir_json expanded internally by
fuchsia_structured_config_*_lib macros, causing assert_no_deps_aspect to fail
with 'type Target is not iterable'. Second, migration_sanity.sh omitted
fuchsia_structured_config_rust_lib from CONVERTIBLE_BAZEL_RULES and omitted
forward_variables_from/concurrent_jobs from GN_ONLY_ATTR_RE, falsely flagging
pre-sentinel binaries that rely on GN-only RBE pool forwarding or unmigrated
dependency trees. Third, migration_sanity.sh and gn_attr_parity.sh did not
record INFO attribute_alignment coverage for standard fuchsia_* to fx_* rule
mappings such as fuchsia_component_manifest to fx_component_manifest, leaving
review_diff's warning un-deduplicated.

## Why not a one-off fix

Any partial or full migration of a Fuchsia component package that uses
fuchsia_structured_config_rust_lib, fx_component_manifest, or GN-only RBE pool
forwarding (concurrent_jobs / forward_variables_from) hits these same three
check false positives across the tree unless build_verification.sh,
migration_sanity.sh, and gn_attr_parity.sh handle these general rule and
attribute patterns.
