# Host rust test args and backtrace parity

- **Learned from:** cq failure on CL Ia48940ae997179ccd0c3494353168001666483d0, patchset 3 (task `src_developer_ffx_plugins_package_blob_decompress`)
- **Date:** 2026-10-26
- **Changed:** `checks/gn_attr_parity.sh`

## Root cause

When migrating a GN target with unit tests (such as `ffx_plugin` or
`rustc_library` with `with_unit_tests = true` and `args = ["env
RUST_LIB_BACKTRACE=0"]`) to `BUILD.bazel`, `gn_attr_parity` did not verify that
`args` on test or `with_unit_tests` targets at `$PLANTER_CHANGE_BASE` were
preserved verbatim in `test_args`/`args` on the migrated Bazel and GN test
targets, nor did it flag `"env RUST_BACKTRACE=0"` in
`host_rustc_test`/`rustc_test` `test_args`/`args` (which GN's `rustc_test.gni`
overrides by appending `"env RUST_BACKTRACE=1"` after `invoker.args`, causing
`anyhow::Error` assertions to include backtraces and fail on CQ).

## Why not a one-off fix

Fixing only a single package leaves all other GN `rustc_test`, `host_test`,
`rustc_library`, `rustc_binary`, and `ffx_plugin` targets with `args = [...]`
(including tests suppressing `anyhow::Error` backtraces via `env
RUST_LIB_BACKTRACE=0`) vulnerable to having their test arguments dropped or
replaced with `env RUST_BACKTRACE=0` during dual-build migration.
