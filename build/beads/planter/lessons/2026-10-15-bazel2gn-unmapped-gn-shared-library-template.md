# Bazel2gn unmapped gn shared library template

- **Learned from:** inner loop friction on [CL 1888673](https://fuchsia-review.googlesource.com/c/fuchsia/+/1888673), patchset 2 (task `pkg-tee-client-api`)
- **Date:** 2026-10-15
- **Changed:** `checks/migration_sanity.sh`, `tools/find_rdeps.sh`

## Root cause

checks/migration_sanity.sh and tools/find_rdeps.sh included plain GN
`shared_library` in `CONVERTIBLE_GN_TEMPLATES`, assuming `fx bazel2gn` can
generate `shared_library()` targets below `## BAZEL2GN SENTINEL`. However,
`build/tools/bazel2gn/constants.go` (`bazelRuleToGNTemplate`) maps `cc_library`
and `fx_cc_library` only to `static_library` or `source_set` (and
`idk_cc_shared_library` / `cc_shared_library_zx` only to `sdk_shared_library` /
`zx_library`), with no rule mapping that emits a plain GN `shared_library()`. As
a result, keeping a plain `shared_library()` above the sentinel to preserve GN
target type parity triggered a false-positive `duplicate_target_above_sentinel`
error.

## Why not a one-off fix

Exempting a single package directory would leave `migration_sanity.sh` and
`find_rdeps.sh` misclassifying every other dual-build package that defines a
plain GN `shared_library()` alongside `bazel2gn`-convertible targets. Aligning
`CONVERTIBLE_GN_TEMPLATES` with `bazel2gn`'s actual rule-to-template mapping in
`build/tools/bazel2gn/constants.go` fixes the contradiction across all
dual-build migrations.
