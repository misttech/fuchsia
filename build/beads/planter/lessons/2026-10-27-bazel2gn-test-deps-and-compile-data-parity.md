# Bazel2gn test deps and compile data parity coverage

- **Learned from:** inner loop friction on a local task (task `src-recovery-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/gn_attr_parity.sh`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When a GN target with `with_unit_tests = true` is decomposed into an explicit
`rustc_test` with `crate = ":<lib>"`, GN-only `test_deps` (such as
`sanitizer_extra_options` targets with no Bazel equivalent) preserved on
`<name>_test` via `# @bazel2gn:raw_attribute:deps += [...]` or `#
@bazel2gn:path_overwrite:` were not recorded as `INFO` `unit_test_parity`
coverage by `gn_attr_parity.sh`, and compile-time `inputs` used only inside
`#[cfg(test)]` that moved to `compile_data` on `<name>_test` (with `#
@bazel2gn:path_overwrite:`) were not checked on companion unit test targets by
`check_compile_data_inputs_info` in `gn_attr_parity.sh` or
`migration_sanity.sh`, causing `review_diff` (`diffengine`) to emit
false-positive `unit_test_parity` and `missing_sources` errors.

## Why not a one-off fix

Hardcoding exemptions for specific libraries or sanitizer labels would fail
whenever any other dual-built library preserves GN-only `test_deps` on
`<name>_test` via `@bazel2gn:raw_attribute` / `@bazel2gn:path_overwrite` or
moves test-only `inputs` to `compile_data` on `<name>_test`; inspecting
companion `<name>_test` targets and `@bazel2gn` directives in
`gn_attr_parity.sh`, `migration_sanity.sh`, and `run_checks.sh` resolves the
entire class of false positives across all packages.
