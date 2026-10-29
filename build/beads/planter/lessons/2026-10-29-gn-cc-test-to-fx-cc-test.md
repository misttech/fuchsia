# GN C++ test() maps to fx_cc_test, not fx_cc_binary(testonly = True)

- **Learned from:** review of https://fxrev.dev/1885900 and https://fxrev.dev/1930546
- **Date:** 2026-10-29
- **Changed:** `prompts/coder.md`, `checks/gn_target_type_parity.sh`, `checks/manifest.json`,
  `checks/migration_sanity.sh`, `checks/gn_attr_parity.sh`, `checks/cq_reachability.sh`,
  `checks/bazel_minimality.sh`, `prompts/reviewers/dual_build_sentinel.md`,
  `prompts/reviewers/target_parity.md`

## Root cause

The coder instructions only offered `fx_cc_binary(testonly = True)` for C++ test binaries, which
bazel2gn converts to `executable()`. Migrating a GN `test()` that way silently changed the GN copy:
`test()` (via `cc_test_executable()`) links with `no-fat-lto-objects-linking`, installs to
`test/<output_name>` in packages and adds a `host_test()` on host, and `executable()` does none of
these. `gn_target_type_parity` reported the type change as a generic ERROR, which the coder disputed
as equivalent, so the regression passed review. `fx_cc_test`
(`//build/bazel/rules/cc:fx_cc_test.bzl`) now exists and bazel2gn converts it to GN `test()`.

## Why not a one-off fix

Every directory with a GN C++ `test()` (several hundred in the tree) hits the same path, and the
generic type-change finding invites disputes. The check now reports `test_became_executable` with a
concrete `fx_cc_test` remediation (only when `fx_cc_test.bzl` exists in the checkout), and the
checks that recognize C++ test binaries treat `fx_cc_test` like a `testonly` `fx_cc_binary`. GN
`cc_test_executable()` keeps the existing `fx_cc_binary(testonly = True)` mapping
(`cc_test_executable_converted`) for now, since switching it would move its GN package path from
`bin/` to `test/`.
