# Bazel2gn generated rustc test for blocked gn test packages

- **Learned from:** inner loop friction on a local task (task `pkg-base-resolver-component-resolver`)
- **Date:** 2026-10-19
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`, `prompts/reviewers/dual_build_sentinel.md`
- **Superseded by:** `2026-10-22-policy-drift-public-visibility-and-test-skip.md`

## Status: superseded

Tests of dual-built targets are now defined in `BUILD.bazel` without
`# @bazel2gn:skip` by default so `bazel2gn` generates their GN copies, and
`bazel2gn` now maps `crate_name` on `rustc_test` directly to GN `output_name`
(`rustBinAttrMap`). Both the `# @bazel2gn:skip` exemption and the
`rustc_test_gn_crate_name` / `# @bazel2gn:raw_overwrite` workaround were
dropped when rebasing onto the current test and `bazel2gn` policy.

## Root cause

While prompts/coder.md, prompts/playbooks/case2_dual_build.md, and
checks/migration_sanity.sh already permitted bazel2gn to generate a standalone
rustc_test below the BAZEL2GN SENTINEL when a blocked GN test component or
package kept above the sentinel depends on it,
prompts/reviewers/dual_build_sentinel.md unconditionally required target-level #
@bazel2gn:skip on every rustc_test and forbade rustc_test below the sentinel,
causing the reviewer seat to reject valid dual-build rustc_test targets and
their attribute-level # @bazel2gn:skip / # @bazel2gn:raw_overwrite output_name
mapping.

## Why not a one-off fix

Any partially migratable Fuchsia test package whose fuchsia_test_package or
sibling components remain above the sentinel due to GN-only dependencies,
test_type, or subpackages while its standalone rustc_test binary migrates to
BUILD.bazel will hit this same reviewer-versus-check contradiction and
rustc_test crate_name-to-output_name mapping requirement across the tree unless
both the sentinel reviewer prompt and deterministic sanity check handle
pre-sentinel-referenced rustc_test binaries uniformly.
