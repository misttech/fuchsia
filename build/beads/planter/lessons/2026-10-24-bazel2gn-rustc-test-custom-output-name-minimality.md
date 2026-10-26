# Bazel2gn rustc test custom output name minimality exemption

- **Learned from:** inner loop friction on CL I8de6659b6ab112b3b500c35f464234d271450ed2, patchset 1 (task `src-connectivity-overnet-lib-core`)
- **Date:** 2026-10-24
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

In `bazel_minimality.sh`, `check_rustc_test_crate_reuse` treated `crate_name` on
crate unit-test `rustc_test` targets as redundant without accounting for all
cases where `bazel2gn` maps `rustc_test.crate_name` to GN `output_name` (such as
preserving a normalized custom test `name`/`output_name` from the base
`BUILD.gn` or setting a disambiguated `<lib_crate_name>_test` binary name in a
dual-build package while still rejecting accidental copies of the library's own
`crate_name`).

## Why not a one-off fix

Allowlisting a single package or target name would fail on any other Rust
library package whose generic target name (such as `core`, `lib`, or `common`)
requires `crate_name = "<crate>_test"` on `rustc_test` so `bazel2gn` emits a
disambiguated `output_name` in `BUILD.gn` without colliding in `root_out_dir`.
