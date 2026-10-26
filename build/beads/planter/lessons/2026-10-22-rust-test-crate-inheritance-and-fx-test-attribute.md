# Rust test crate inheritance and fx test attribute parity

- **Learned from:** inner loop friction on a local task (task `swd-pkg-system-update-checker`)
- **Date:** 2026-10-22
- **Changed:** `checks/gn_attr_parity.sh`, `tools/run_checks.sh`

## Root cause

review_diff expands crate = ":<target>" when comparing rustc_test dependencies
against GN but does not inherit @bazel2gn:path_overwrite directives from the
referenced crate target, flags removed obsolete '# Kept in GN because ...'
retention comments as dropped_comment when a previously retained GN target is
migrated to Bazel, and flags GN test_specs.log_settings.max_severity when
carried over as max_log_severity on fx_test rather than fx_unittest_package.

## Why not a one-off fix

Any dual-build package whose rustc_test uses crate = ":<bin_or_lib>" on a target
with @bazel2gn:path_overwrite, removes an obsolete GN-retention blocker comment
upon migrating the target to Bazel, or sets max_log_severity on an explicit
fx_test target will hit the same false-positive review_diff warnings unless
gn_attr_parity and run_checks record general AST-based INFO coverage for all
three patterns.
