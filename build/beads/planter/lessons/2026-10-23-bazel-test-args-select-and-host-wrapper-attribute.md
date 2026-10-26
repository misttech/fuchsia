# Bazel test args select and host wrapper attribute alignment

- **Learned from:** inner loop friction on a local task (task `src-connectivity-overnet-lib-core`)
- **Date:** 2026-10-23
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

The static diffengine (review_diff) flags GN target `args` as not carried over
to Bazel when `args` on a migrated `rustc_test` or binary rule is expressed with
a `select({...})` expression or carried via `test_args` on a companion
`wrap_host_rust_test` / `host_*_test` wrapper, and `gn_attr_parity.sh` only
recorded non-blocking `INFO` `attribute_alignment` coverage for `output_name`,
`rustc_library_with_features`, and `build_flags`.

## Why not a one-off fix

Any GN test or binary target that passes platform-conditional `args` (such as
`if (is_host) { args = [...] }` translated to `args = select({...})` on
`rustc_test` and `test_args = [...]` on `wrap_host_rust_test`) will trigger the
same false-positive `review_diff` `attribute_alignment` warning unless
`gn_attr_parity` and `run_checks` systemically recognize `args` carried over via
Bazel `args`/`select()`, companion host-test `test_args`, or `bazel2gn` output.
