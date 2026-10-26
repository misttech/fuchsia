# Generated third party vendor build hygiene

- **Learned from:** cq failure on CL I6383cb2795407c205b78c8f935cbf3bcfd2b815e, patchset 1 (task `pkg-mock-omaha-server-fuchsia-test`)
- **Date:** 2026-10-20
- **Changed:** `checks/bazel_minimality.sh`, `tools/run_checks.sh`

## Root cause

When a migration updated crate_annotation_overwrites.bzl and regenerated a
third_party/rust_crates/vendor/*/BUILD.bazel file,
check_starlark_copyright_headers in checks/bazel_minimality.sh and review_diff
did not exempt auto-generated crate_universe vendor BUILD.bazel files from
copyright_header and default_visibility checks, causing the coder to hand-edit
the vendor BUILD.bazel file to add a Fuchsia copyright header and remove
package-level default_visibility.

## Why not a one-off fix

Fixing only a single vendored crate directory is insufficient because any
migration that updates crate_annotation_overwrites.bzl and regenerates a
third_party/rust_crates/vendor/*/BUILD.bazel file will encounter the same
false-positive copyright_header and default_visibility findings unless
checks/bazel_minimality.sh and tools/run_checks.sh systemically exempt
crate_universe vendor BUILD.bazel files and reject manual header or visibility
edits to them.
