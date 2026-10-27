# Go library test sources decomposition parity

- **Learned from:** inner loop friction on a local task (task `pkg-htt-paver`)
- **Date:** 2026-10-19
- **Changed:** `prompts/playbooks/case2_dual_build.md`, `tools/run_checks.sh`

## Root cause

In GN, go_library targets commonly list *_test.go files directly in sources
(which GN's Go builder ignores for non-test library builds and passes to go_test
via embed), whereas Bazel @io_bazel_rules_go go_library compiles all srcs into
the library archive and requires moving *_test.go files into the embedding
host_go_test or go_test srcs. While reviewer prompts accounted for this
decomposition, run_checks.sh's review_diff runner forwarded unfiltered
diffengine missing_sources findings on go_library when *_test.go files moved to
a same-package go_test/host_go_test, and the dual-build playbook lacked explicit
guidance on moving *_test.go files out of go_library.srcs.

## Why not a one-off fix

Many Fuchsia Go libraries across host testing, tools, and netstack list
*_test.go files inside go_library.sources in BUILD.gn; filtering false-positive
missing_sources findings in tools/run_checks.sh when *_test.go files are present
in an embedding go_test/host_go_test target in the same BUILD.bazel and
documenting the rule in the dual-build playbook prevents inner-loop friction
across all Go package migrations.
