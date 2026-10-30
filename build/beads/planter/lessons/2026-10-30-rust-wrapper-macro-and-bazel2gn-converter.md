# Rust wrapper macro and bazel2gn converter minimality

- **Learned from:** human review on [CL 1932168](https://fuchsia-review.googlesource.com/c/fuchsia/+/1932168), patchset 5 (task `rustc-staticlib-b2gn`)
- **Date:** 2026-10-30
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

The existing bazel_minimality check did not audit newly added Fuchsia Rust
wrapper macros (such as rustc_staticlib) or their bazel2gn converter mappings
for custom Starlark rule() wrappers over @rules_rust rules, deprecated
with_*_unit_tests/test_deps/non_test_deps/hdrs attributes, or unprefixed
bazel2gn passthrough attributes (quiet_clippy and disable_rustdoc instead of
gn_quiet_clippy and gn_disable_rustdoc).

## Why not a one-off fix

Fixing only a single consumer target or macro revision without a deterministic
check in bazel_minimality.sh would allow future Rust wrapper macros and bazel2gn
rule conversions to reintroduce custom provider-rewriting wrapper rules,
deprecated with_*_unit_tests generation, or unprefixed GN-only passthrough
attributes.
