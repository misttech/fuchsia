# Dual build sentinel testonly component binary and local gn only deps

- **Learned from:** inner loop friction on [CL 1885780](https://fuchsia-review.googlesource.com/c/fuchsia/+/1885780), patchset 2 (task `pkg-inspect-runtime`)
- **Date:** 2026-10-15
- **Changed:** `checks/migration_sanity.sh`

## Status: partly outdated

`fuchsia_structured_config_rust_lib` is no longer a non-convertible GN-only
template: Bazel has `fuchsia_structured_config_rust_lib()` in
`//build/bazel/rules/components:structured_config.bzl` and bazel2gn translates
it. Migrate it to Bazel instead of keeping it above the sentinel.
`fuchsia_structured_config_cpp_elf_lib` is still GN-only. The testonly binary
exemption below still applies.

## Root cause

In checks/migration_sanity.sh, duplicate_target_above_sentinel only exempted
testonly = true binaries above the sentinel when their template was 'executable'
and they were directly referenced by fuchsia_unittest_package,
fuchsia_test_package, fuchsia_test_component, or bootfs_test, ignoring
rustc_binary/go_binary/python_binary and intermediate fuchsia_component targets.
Furthermore, gn_blocked_dep only inspected external '//' dependencies and
ignored same-package ':target' dependencies resolving to non-convertible GN-only
templates (such as fuchsia_structured_config_rust_lib) above the sentinel.

## Why not a one-off fix

Exempting a single target name or directory would fail whenever any other
dual-build package retains a testonly rustc_binary/go_binary/python_binary
packaged via a GN fuchsia_component or has a pre-sentinel target depending on a
local non-convertible GN target such as fuchsia_structured_config_rust_lib or
fuchsia_structured_config_cpp_elf_lib.
