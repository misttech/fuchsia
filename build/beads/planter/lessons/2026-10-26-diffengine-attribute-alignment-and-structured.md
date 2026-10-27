# Diffengine attribute alignment and structured config fidl validation

- **Learned from:** inner loop friction on CL I9492e89e2c7d9dbc88feba65ec6760313bdd095c, patchset 1 (task `src_sys_time_httpsdate_time_source`)
- **Date:** 2026-10-26
- **Changed:** `tools/run_checks.sh`

## Root cause

While checks/build_verification.sh and checks/gn_attr_parity.sh already handled
structured config _fidl_internal_validate_ir_json exclusions and
fx_component/fx_unittest_package attribute alignment, tools/run_checks.sh
downgrade_check_cfg_diff_findings only downgraded max_severity
attribute_alignment warnings when max_log_severity was present in BUILD.bazel,
leaving log_settings and test_specs attribute_alignment warnings un-downgraded
when review_diff ran without matching gn_attr_parity base targets.

## Why not a one-off fix

Any Fuchsia test package migrating GN test_specs.log_settings.max_severity to
Bazel fx_unittest_package or fx_test max_log_severity can trigger standalone
review_diff attribute_alignment warnings for log_settings and test_specs, so
downgrade_check_cfg_diff_findings in tools/run_checks.sh must recognize all
three attributes generically.
