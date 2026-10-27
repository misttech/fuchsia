# Dual built rustc dylib blobfs duplication in assembled fx packages

- **Learned from:** cq failure on CL I9492e89e2c7d9dbc88feba65ec6760313bdd095c, patchset 2 (task `src_sys_time_httpsdate_time_source`)
- **Date:** 2026-10-26
- **Changed:** `checks/migration_sanity.sh`

## Root cause

checks/migration_sanity.sh gated find_uncollected_rust_dylib_dep on
RUST_DYLIB_DEBUG_SYMBOLS_SUPPORTED, so once
build/bazel/debug_symbols/aspects.bzl added rust_dylib_library, migration_sanity
stopped detecting transitive rustc_dylib dependencies on assembled packages and
forced switching GN fuchsia_package targets bridged via
bundles/assembly/bazel_inputs to Bazel fx_package targets. Because the
Bazel-built rustc_dylib and the GN-built rustc_dylib shared by remaining GN
packages in the assembled product image have distinct content hashes, BlobFS
includes two copies of the shared dylib and exceeds release binary size creep
budgets.

## Why not a one-off fix

Fixing only a single package leaves migration_sanity requiring fx_package
assembly switchovers for every other assembled package whose binary transitively
links a dual-built rustc_dylib (such as //src/storage/lib/vfs/rust:vfs_dylib),
causing identical BlobFS dylib duplication and size creep failures on release
builders.
