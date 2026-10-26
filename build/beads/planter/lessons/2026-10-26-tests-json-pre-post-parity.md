# A migration must not drop tests from tests.json

- **Learned from:** gap in the test coverage checks of dual-build migrations
  (https://fxbug.dev/574121341)
- **Date:** 2026-10-26
- **Changed:** `checks/tests_json_parity.sh`, `checks/manifest.json`,
  `prompts/coder.md`

## Root cause

`cq_reachability.sh`, `migration_sanity.sh` and build_verification's
`bazel_tests_exported` step only look at the tests the change still defines:
they check that each one is wired into a `group("tests")` or a
`bazel_test_suite` and reaches `<build dir>/tests.json`. A test the migration
loses entirely leaves nothing behind for them to check, e.g. a GN test package
deleted without a Bazel replacement, a `group("tests")` entry or a parent
`:tests` reference removed, or a Bazel `test_suite` that no longer lists a
test. Such a test silently stops running in `fx test` and infra.

## Remediation

`tests_json_parity.sh` compares the tests in `<build dir>/tests.json` before
and after the change and reports each test that is gone as
`test_dropped_from_tests_json`. tests.json comes from `fx gen` (including Bazel
queries for exported Bazel tests), so the baseline is a real regeneration of the
changed files at `$PLANTER_CHANGE_BASE`, cached per change base and build
configuration. tests.json only lists tests reachable from the configured roots,
which rarely include the migrated packages, so both generations add the changed
packages' `:tests` groups to args.gn. A host test that moves from GN to Bazel
changes its tests.json name but keeps its label, so host tests also match by
label. Device tests match only by package URL, because every test component of a
GN `fuchsia_test_package` shares the package's label.
