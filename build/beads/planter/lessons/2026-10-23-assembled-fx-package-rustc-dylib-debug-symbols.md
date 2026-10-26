# Assembled fx package rustc dylib debug symbols remediation

- **Learned from:** inner loop friction on CL I3c18384211358d98607eb6bdc50eb4dd1b8df163, patchset 2 (task `swd-pkg-system-update-checker`)
- **Date:** 2026-10-23
- **Changed:** `checks/bazel_minimality.sh`, `checks/commit_message_format.sh`, `checks/migration_sanity.sh`

## Root cause

When CQ failed export_last_build_debug_symbols on an assembled fx_package whose
binary depends on a rustc_dylib target, the coder edited
build/bazel/debug_symbols/aspects.bzl directly and disputed bazel_minimality
out_of_scope_package_modified because its remediation only described legacy
host-tool visibility rather than keeping the assembled fuchsia_package in
BUILD.gn with its bundles/assembly/bazel_inputs bridge until aspects.bzl
collects rust_dylib_library upstream.

## Why not a one-off fix

Reverting build/bazel/debug_symbols/aspects.bzl in a single package migration
without teaching bazel_minimality, migration_sanity, and commit_message_format
how to remediate and represent packages whose assembled fuchsia_package stays
above the BAZEL2GN SENTINEL due to uncollected rust_dylib_library debug symbols
would leave future package migrations with the same dispute and downstream
unmatched_target or commit subject friction.
