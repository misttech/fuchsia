# Partial dual build test and target pairing parity

- **Learned from:** inner loop friction on a local task (task `pkg-base-resolver-component-resolver`)
- **Date:** 2026-10-19
- **Changed:** `checks/cq_reachability.sh`, `checks/migration_sanity.sh`, `prompts/coder.md`, `prompts/playbooks/case2_dual_build.md`, `tools/run_checks.sh`
- **Superseded by:** `2026-10-22-policy-drift-public-visibility-and-test-skip.md` (test skips only)

## Status: partly superseded

The `# @bazel2gn:skip` requirement on `rustc_test` targets no longer exists:
`bazel2gn` now generates GN copies of tests in dual-build packages by default.
Pre-sentinel GN test package reachability (`unpackaged_rust_device_test`) and
exact-name target pairing parity when pre-sentinel GN targets shift positional
`diffengine` pairing still apply.

## Root cause

When a GN test component or package must remain above the BAZEL2GN SENTINEL in
BUILD.gn (due to test_type, subpackages, or GN-only dependencies) and depends on
a bazel2gn-generated rustc_test binary, migration_sanity's second pass
(rust_unit_test_layout), cq_reachability (unpackaged_rust_device_test), and
diffengine (two_case_rule and attribute_alignment) did not exempt test binaries
referenced by pre-sentinel GN test components/packages from requiring #
@bazel2gn:skip and Bazel fx_test packaging. In addition, when convertible
targets of a template remain above the sentinel alongside migrated targets of
the same template below the sentinel, diffengine pairs base BUILD.gn and
BUILD.bazel targets positionally rather than by exact name first, producing
false-positive missing_sources, dep_alignment, attribute_alignment, and
unmatched_target errors on targets whose bazel2gn-generated definitions below
the sentinel match the base BUILD.gn.

## Why not a one-off fix

Exempting only a single package directory would leave every future partial
dual-build migration where pre-sentinel GN test packages depend on dual-built
test binaries or where unmigrated pre-sentinel targets share a template with
migrated targets blocked by contradictory rust_unit_test_layout,
unpackaged_rust_device_test, and positional diffengine pairing errors.
