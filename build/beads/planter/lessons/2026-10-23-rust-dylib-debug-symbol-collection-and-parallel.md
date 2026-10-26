# Rust dylib debug symbol collection and parallel evolution check sync

- **Learned from:** inner loop friction on CL I4eb1208833ce04e0f6df1ff341fbc538e588d2cc, patchset 11 (task `pkg-http-client`)
- **Date:** 2026-10-23
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When a parallel human-review machinery evolution ran alongside the coder,
tools/run_checks.sh executed static checks (migration_sanity.sh) before
build_verification.sh while the evolution was still in progress, and did not
wait for the evolution to finish or re-run the static checks after
build_verification.sh completed and the evolved check merged. As a result,
run_checks.sh reported stale pre-evolution duplicate_gn_fuchsia_package errors
on assembled fuchsia_package targets that depend on rustc_dylib targets (before
collect_debug_symbols_manifest_aspect in //build/bazel/debug_symbols/aspects.bzl
supports rust_dylib_library), causing the coder to dispute a finding that the
newly merged check already exempted.

## Why not a one-off fix

Exempting a single package directory or relying on manual disputes does not
prevent stale pre-evolution static check findings whenever a parallel machinery
evolution merges while build_verification.sh is running, nor does it document
kept GN fuchsia_package targets with transitive rustc_dylib dependencies across
other assembled packages. Updating tools/run_checks.sh to wait for any
in-progress evolution after build_verification and re-run static checks whenever
check scripts change mid-run, and hardening migration_sanity.sh's
rust_dylib_library detection and INFO reporting, prevents this friction across
all packages.
