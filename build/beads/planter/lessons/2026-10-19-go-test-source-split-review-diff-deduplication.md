# Go test source split review diff deduplication

- **Learned from:** inner loop friction on a local task (task `pkg-htt-paver`)
- **Date:** 2026-10-19
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When migrating a GN go_library whose sources list includes *_test.go files into
a Bazel go_library plus host_go_test/go_test with embed, migration_sanity
emitted an INFO missing_sources finding to pre-populate planter's (category,
file, line, message) deduplication set before review_diff (diffengine) runs, but
only inside the has_sentinel branch and without explaining in its remediation
that this INFO entry already suppresses planter's post-report review_diff
missing_sources ERROR, causing coders that inspected run_checks.sh or ran
planter diff directly to report false machinery_conflict and missing_sources
entries in disputed_findings.

## Why not a one-off fix

Fixing or waiving the finding for a single Go package directory would leave
every other Go library + test migration susceptible to either un-deduplicated
review_diff missing_sources errors (when no BAZEL2GN SENTINEL is present) or
repeated inner-loop disputed_findings friction from coders misinterpreting
standalone planter diff output.
