# Dual build rustc test crate reuse and kept gn test package scaffolding

- **Learned from:** human review on CL If2ee09fc873ef597f74c3f851d001680998678d2, patchset 4 (task `pkg-dns-lib`)
- **Date:** 2026-10-21
- **Changed:** `checks/bazel_minimality.sh`, `checks/migration_sanity.sh`

## Root cause

First, `checks/migration_sanity.sh` previously emitted an
`unmigrated_dual_build_test_component` error whenever a `fuchsia_component` or
`fuchsia_test_component` remained above `## BAZEL2GN SENTINEL` in `BUILD.gn` for
a kept GN test package with unmigrated dependencies, forcing the coder to add
dead `# @bazel2gn:skip` `fx_packaged_binary`, `fx_component_manifest`, and
`fx_test_component` targets (and out-of-scope `.shard.cml` `exports_files`
edits) to `BUILD.bazel` even though the enclosing test package could not move to
Bazel and nothing in `BUILD.bazel` consumed the test component. Second,
`checks/migration_sanity.sh` instructed dual-built `rustc_test` targets to share
`_<LIB>_SRCS` and `_<LIB>_DEPS` variables instead of `crate = ":<lib>"`, and
`checks/bazel_minimality.sh` (`check_rustc_test_crate_reuse`) suppressed
`rustc_test_repeats_library_srcs` whenever a preceding comment mentioned `crate`
even without `non_test_deps` or `testutils`, missing that `bazel2gn` natively
supports `crate = ":<lib>"` on `rustc_test` and inlines the crate's sources and
dependencies into the generated GN target.

## Why not a one-off fix

Fixing only a single package leaves `migration_sanity.sh` and
`bazel_minimality.sh` enforcing and permitting these anti-patterns across every
dual-build Rust package whose GN test package remains above the sentinel due to
unmigrated dependencies. Updating `checks/migration_sanity.sh` to remove
`unmigrated_dual_build_test_component`, reject unreferenced `# @bazel2gn:skip`
`fx_component`/`fx_test_component`/`fx_unittest_component` duplicates in
`unused_skipped_duplicate_target`, and recommend `crate = ":<lib>"` for
dual-built unit tests, while updating `checks/bazel_minimality.sh` to
unconditionally flag `rustc_test_repeats_library_srcs` when no
`non_test_deps`/`testutils` dependency swap exists, prevents both defects
systemically across all future migrations.
