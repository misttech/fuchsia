# Dual build bazel2gn sync and host tool freshness

- **Learned from:** cq failure on CL I4eb1208833ce04e0f6df1ff341fbc538e588d2cc, patchset 6 (task `pkg-http-client`)
- **Date:** 2026-10-22
- **Changed:** `checks/build_verification.sh`, `checks/migration_sanity.sh`

## Root cause

When upstream changes to //build/tools/bazel2gn landed during a rebase (such as
automatically emitting source_root = "src/main.rs" when inlining a rustc_binary
crate into rustc_test), static checks in migration_sanity.sh neither flagged
redundant # @bazel2gn:raw_attribute:source_root annotations on rustc_test(crate
= ...) targets nor ran bazel2gn --check_only, and build_verification.sh ran fx
host-tool shac check against a stale <build_dir>/host_x64/bazel2gn binary
because default fx build does not rebuild host_x64/bazel2gn when it already
exists on disk.

## Why not a one-off fix

Fixing only a single package's BUILD.bazel and BUILD.gn leaves every future
dual-build migration vulnerable to verify_bazel2gn CQ failures whenever
//build/tools/bazel2gn changes across a rebase or when redundant
raw_attribute:source_root annotations are placed on crate-referencing rustc_test
targets.
