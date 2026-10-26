# Rust target attribute and raw attribute alignment parity

- **Learned from:** inner loop friction on a local task (task `pkg-cache-bin`)
- **Date:** 2026-10-23
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

gn_attr_parity.sh only recorded combined Target Attributes INFO coverage (for
configs, with_unit_tests decomposition, and extra GN attributes) when tmpl ==
'rustc_library_with_features' and omitted 'configs mapped to Bazel equivalents'
and 'added crate_name=<val> in Bazel' (when GN used name = '<val>'). As a
result, review_diff flagged a false-positive attribute_alignment warning on
migrated rustc_binary/rustc_* targets that combine crate_name,
build_flags/lint_config, decomposed unit tests, and @bazel2gn:raw_attribute
directives. Additionally, migration_sanity.sh did not treat
fuchsia_structured_config_values2 consumers as blocking pre-sentinel
fuchsia_component_manifest/fuchsia_component targets.

## Why not a one-off fix

Hardcoding a single binary target or crate name would leave all other migrated
Rust binaries and libraries that combine crate_name (from GN name/output_name),
build_flags/lint_config (from GN configs), decomposed rustc_test unit tests, and
@bazel2gn:raw_attribute statements (such as _concurrent_jobs) exposed to
false-positive review_diff attribute_alignment warnings, and would leave other
packages using fuchsia_structured_config_values2 blocked by migration_sanity.
