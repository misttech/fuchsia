# Bazel subpackage export relocation and local gn template unit test parity

- **Learned from:** inner loop friction on a local task (task `ffx-lib-pkg-pbms`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_attr_parity.sh`, `checks/migration_sanity.sh`

## Root cause

First, check_preexisting_bazel_exports_and_targets in checks/migration_sanity.sh
flagged dropped_preexisting_bazel_export whenever an ancestor BUILD.bazel
removed a nested path from exports_files(...), without checking whether an
intermediate directory along that path is now a Bazel subpackage with its own
BUILD.bazel (which makes exporting the subpackage path from the ancestor
BUILD.bazel illegal in Bazel and requires exporting the relative path from the
subpackage's BUILD.bazel instead). Second, targets() in checks/gn_attr_parity.sh
skipped same-file GN template(...) blocks when parsing target bodies, so targets
instantiated via a local wrapper template did not inherit the template's
test_deps and attributes when emitting INFO unit_test_parity coverage for
review_diff deduplication.

## Why not a one-off fix

Hardcoding a single package or file path would fail whenever any other migration
creates a new BUILD.bazel subpackage under an ancestor package that previously
exported nested files across that directory boundary, or whenever any BUILD.gn
defines a local wrapper template with test_deps and with_unit_tests.
