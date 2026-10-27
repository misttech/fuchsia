# Go library test source decomposition in review diff

- **Learned from:** inner loop friction on a local task (task `pkg-htt-paver`)
- **Date:** 2026-10-19
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

Planter executes manifest checks from checks/manifest.json directly via its Go
engine and then runs its internal review_diff check, deduplicating review_diff
findings against manifest check findings by (category, file, line, message).
Filtering go_library *_test.go missing_sources only inside tools/run_checks.sh
left the Go engine's review_diff check unfiltering the ERROR when *_test.go
files moved from GN go_library.sources to a same-package host_go_test/go_test
with embed.

## Why not a one-off fix

Any Fuchsia Go package migration where GN go_library lists *_test.go files in
sources alongside library files will move those test files into a Bazel
host_go_test or go_test target with embed, so checks/migration_sanity.sh must
emit the matching INFO missing_sources finding for all Go packages so both
run_checks.sh and the Go engine's review_diff deduplication treat the
decomposition as non-blocking.
