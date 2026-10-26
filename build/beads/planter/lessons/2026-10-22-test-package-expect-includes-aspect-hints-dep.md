# Test package expect includes aspect hints dep alignment deduplication

- **Learned from:** inner loop friction on CL If6a81435bf8531155538049cfca7fc9444b5835f, patchset 1 (task `src_sys_time_timekeeper_persistence`)
- **Date:** 2026-10-22
- **Changed:** `checks/gn_attr_parity.sh`, `tools/run_checks.sh`

## Root cause

When check_dep_alignment_info in checks/gn_attr_parity.sh accounted for GN
test-package expect_includes dependencies moved to aspect_hints on unit_tests
binaries, it merged hinted_deps into bazel_deps prior to matching and rewrote
the finding message clause to 'moved to aspect_hints of the unit_tests
binaries...'. Because planter and run_checks.sh deduplicate review_diff findings
against check findings by exact (category, file, line, message) tuple, the
custom message failed to match review_diff's 'unmatched in GN: [...]' message,
leaving review_diff's warning un-deduplicated and blocking.

## Why not a one-off fix

Hardcoding a target name or directory exemption would only silence this single
test package, whereas any Fuchsia test package migrating GN expect_includes
dependencies (such as tmp_storage or other shard targets) onto unit_tests binary
aspect_hints in fx_unittest_package or fx_unittest_component triggers the same
review_diff dep_alignment message mismatch.
