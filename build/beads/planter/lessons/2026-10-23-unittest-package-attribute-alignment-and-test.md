# Unittest package attribute alignment and test footer verification

- **Learned from:** inner loop friction on a local task (task `swd-pkg-multi-universal-tool`)
- **Date:** 2026-10-23
- **Changed:** `checks/build_verification.sh`, `checks/gn_attr_parity.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

When a GN fuchsia_unittest_package in HEAD (either hand-written with default
names or previously generated below the BAZEL2GN SENTINEL by bazel2gn with
synthesized package_name = "<name>-gn" and component_name = "<name>") is
replaced by or marked # @bazel2gn:skip on fx_unittest_package(name = "<name>"),
fx_unittest_package defaults package_name and component_name to name and does
not accept a component_name attribute. gn_attr_parity recorded INFO coverage for
the fuchsia_unittest_package -> fx_unittest_package rule mapping itself, but did
not emit INFO attribute_alignment coverage for the omitted default/<name>-gn
package_name and component_name attributes, causing review_diff to emit a
false-positive attribute_alignment WARNING. Additionally, build_verification
skipped bazel_tests_export_notes when PLANTER_SKIP_BUILD=1 was set and referred
to tests_run instead of test_footers, so slice coders running with --skip-build
were not warned that exported tests missing from <build dir>/tests.json cannot
be verified with fx test in test_footers.

## Why not a one-off fix

Suppressing the warning or hardcoding an exemption for a single package
directory would leave every other dual-build package whose pre-existing BUILD.gn
had a bazel2gn-synthesized or default-named fuchsia_unittest_package blocked by
the same review_diff attribute_alignment warning whenever # @bazel2gn:skip is
added and BUILD.gn is regenerated, and would leave future slice migrations
recording fx test footers for unconfigured tests missing from tests.json.
