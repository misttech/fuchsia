# Generated third party crate universe build hygiene coverage

- **Learned from:** inner loop friction on CL I6383cb2795407c205b78c8f935cbf3bcfd2b815e, patchset 1 (task `pkg-mock-omaha-server-fuchsia-test`)
- **Date:** 2026-10-21
- **Changed:** `checks/bazel_minimality.sh`, `tools/run_checks.sh`

## Root cause

When a migration updates crate_universe annotations and regenerates a
third-party Rust crate BUILD.bazel file, review_diff emits copyright_header
('Missing <year> Fuchsia copyright header in BUILD.bazel') and package_hygiene
('package() must not specify default_visibility; set visibility explicitly per
target') findings. check_generated_vendor_build_hygiene in
checks/bazel_minimality.sh only recorded INFO coverage under category
'default_visibility' and yearless copyright_header messages, while
tools/run_checks.sh masked the mismatch locally by downgrading all review_diff
findings on generated crate files.

## Why not a one-off fix

Exempting a single third-party crate directory or relying on local severity
downgrades in run_checks.sh leaves outer-loop review_diff deduplication broken
for every future migration that regenerates crate_universe BUILD.bazel files via
crate_annotation_overwrites.bzl.
