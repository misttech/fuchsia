# Bazel2gn raw attribute and decomposed unit test diff alignment

- **Learned from:** inner loop friction on a local task (task `pkg-blobfs-ramdisk`)
- **Date:** 2026-10-21
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

Planter's diffengine emits WARNING findings for dep_alignment when a dual-build
target preserves a GN-only dependency via `# @bazel2gn:raw_attribute:deps +=
[...]` or `# @bazel2gn:path_overwrite:...`, and for attribute_alignment when
`rustc_env` (with `@bazel2gn:value_overwrite` or `@bazel2gn:raw_overwrite`) or
`lint_config` is combined in the same `; `-joined message with `GN
with_unit_tests decomposed into explicit test target`. Previously,
`gn_attr_parity.sh` only recognized `third_party_target_map.json` mappings in
`check_dep_alignment_info` (missing `@bazel2gn:raw_attribute` and
`@bazel2gn:path_overwrite` directives), and `migration_sanity.sh` only combined
`rustenv_note` and `lint_note` without `GN with_unit_tests decomposed into
explicit test target`, so planter's finding deduplicator did not match and
suppress those two false-positive `review_diff` warnings.

## Why not a one-off fix

Hardcoding a single target or package path would leave every other dual-build
Rust or C++ target that uses `# @bazel2gn:raw_attribute:deps += [...]` for
GN-only component dependencies or combines `rustc_env`/`lint_config` with
decomposed `with_unit_tests` targets blocked on false-positive `review_diff`
`dep_alignment` and `attribute_alignment` warnings. Extending
`gn_attr_parity.sh` and `migration_sanity.sh` to parse general
`@bazel2gn:raw_attribute` and `@bazel2gn:path_overwrite` dependency directives
and emit all ordered combinations of `rustc_env`, `lint_config`, and decomposed
`with_unit_tests` alignment notes resolves the whole class of false positives
across the repository.
