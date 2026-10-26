# Gn test shared visibility narrowing parity

- **Learned from:** inner loop friction on a local task (task `src-developer-ffx-lib-diagnostics-analytics`)
- **Date:** 2026-10-23
- **Changed:** `checks/manifest.json`, `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

When a GN library with `with_unit_tests = true` is migrated to a Bazel library
and a standalone test target (`host_rustc_test` or `rustc_test`), GN's
`rustc_library.gni` previously shared `visibility` between the library and its
generated `<name>_test` target, so parent packages that only depended on
`:<name>_test` in `group("tests")` had to be listed in the library's GN
`visibility`. While `visibility_audit.sh` excluded such `gn_only_pkgs` from
`get_preserved_gn_visibility` and rejected keeping them on the library with
`false_positive_rdep_visibility`, `tools/find_rdeps.sh` omitted
`gn_only_ref_packages` from its `get_preserved_gn_visibility` filter and
`visibility_audit.sh` did not record non-blocking `INFO` `visibility_narrowed`
coverage for `review_diff`, trapping the coder between `visibility_audit` ERROR
and `review_diff` WARNING.

## Why not a one-off fix

Hardcoding a single package or target would not fix the general conflict across
all Fuchsia migrations where a library's pre-migration GN `visibility` included
callers of `:tests`, `:benchmarks`, or a split-out `<target>_test` unit test
target, or overbroad GN wildcards narrowed to satisfy `visibility_audit`.
