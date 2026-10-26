# Test specs max log severity attribute alignment

- **Learned from:** inner loop friction on a local task (task `src_sys_time_timekeeper_persistence`)
- **Date:** 2026-10-22
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

When a GN test package (such as fuchsia_unittest_package or
fuchsia_test_package) sets test_specs = { log_settings = { max_severity = "..."
} } and is migrated to a Bazel fx_test or fx_unittest_package setting
max_log_severity = "...", diffengine's flat line-based GN attribute parser
extracts the nested scope keys log_settings and max_severity as top-level GN
target attributes. Because only test_specs and max_log_severity are in
diffengine's ignored-attribute table and neither gn_attr_parity.sh nor
run_checks.sh recorded INFO attribute_alignment coverage for the
log_settings.max_severity -> max_log_severity mapping, review_diff emitted a
false-positive WARNING.

## Why not a one-off fix

Suppressing this warning only for a single package directory would leave every
future GN test package migration that translates
test_specs.log_settings.max_severity into max_log_severity on fx_test or
fx_unittest_package blocked by the same false-positive review_diff
attribute_alignment warning.
