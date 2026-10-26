# Assembled fx package Debug Symbol Collection for rustc dylib Dependencies

- **Learned from:** cq failure on CL I4eb1208833ce04e0f6df1ff341fbc538e588d2cc, patchset 11 (task `pkg-http-client`)
- **Date:** 2026-10-23
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

collect_debug_symbols_manifest_aspect in //build/bazel/debug_symbols/aspects.bzl
only collects debug symbols for rust_binary, rust_test, and rust_shared_library
rules and omits rust_dylib_library (used by rustc_dylib). When migration_sanity
required migrating an assembled fuchsia_package whose component binary depends
on a rustc_dylib target to fx_package in BUILD.bazel and wiring it into
bundles/assembly/BUILD.bazel, the packaged Rust dylib .so entered elf_sizes.json
on assembled product images without a corresponding debug_symbols.json entry,
causing export_last_build_debug_symbols to fail on CQ.

## Why not a one-off fix

Any Fuchsia package wired into product assembly whose component binary depends
directly or transitively on a rustc_dylib target will trigger
export_last_build_debug_symbols failures on release/assembly builders if
migrated from GN fuchsia_package to Bazel fx_package before aspects.bzl collects
rust_dylib_library. Updating migration_sanity to detect reachable rustc_dylib
dependencies when aspects.bzl lacks rust_dylib_library both prevents premature
fx_package switchover and allows keeping fuchsia_package in BUILD.gn across all
package migrations.
