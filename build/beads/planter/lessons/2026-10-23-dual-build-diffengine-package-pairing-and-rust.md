# Dual build diffengine package pairing and rust config alignment

- **Learned from:** inner loop friction on a local task (task `swd-pkg-cache`)
- **Date:** 2026-10-23
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

First, when a package defines a Bazel fx_package whose GN counterpart uses a
rule not classified by diffengine Pass 1 in the package family (such as
fuchsia_system_package) while an unmigrated GN fuchsia_test_package or
fuchsia_unittest_package remains above the sentinel, diffengine Pass 2 pairs the
fx_package with the GN test package and flags false-positive dep_alignment
warnings on the GN test package's deps and attribute_alignment warnings on
package_name and test_specs.log_settings.max_severity. Second, in
gn_attr_parity.sh, parse_gn_top_attrs only extracted quoted string literals from
GN configs += assignments (omitting unquoted config variable expressions like
_concurrent_jobs.configs) and check_rust_target_attr_alignment_info did not emit
INFO coverage for diffengine's configs ([...]) != lint_config ([...]) comparison
when a rustc_* target sets both build_flags/raw_attribute configs and
lint_config.

## Why not a one-off fix

Suppressing these findings only for a single package directory would leave every
future dual-build migration that converts fuchsia_system_package or
resource-only fx_package targets alongside retained GN test packages, or that
migrates rustc_* targets combining build_flags, lint_config, and
@bazel2gn:raw_attribute config appends, blocked on the same false-positive
review_diff warnings.
