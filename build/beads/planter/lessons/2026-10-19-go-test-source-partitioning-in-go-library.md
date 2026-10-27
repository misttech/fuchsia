# Go test source partitioning in go library migrations

- **Learned from:** inner loop friction on a local task (task `pkg-htt-paver`)
- **Date:** 2026-10-19
- **Changed:** `prompts/panel.json`, `prompts/reviewers/target_parity.md`

## Root cause

The target_parity reviewer seat required 1:1 source list parity between the
original GN target and the migrated Bazel target without accounting for Go rules
differences: GN go_library commonly includes *_test.go files in sources (which
GN filters out for non-test builds and passes to go_test), whereas Bazel
@io_bazel_rules_go go_library compiles all srcs into the library archive and
requires *_test.go files to move into host_go_test/go_test srcs with embed.

## Why not a one-off fix

Many Fuchsia Go packages list *_test.go files directly inside
go_library(sources) in BUILD.gn; without updating the target_parity reviewer
seat to recognize moving *_test.go files from go_library to host_go_test/go_test
srcs, every Go library + test migration will hit the same false-positive
missing_sources review error.
