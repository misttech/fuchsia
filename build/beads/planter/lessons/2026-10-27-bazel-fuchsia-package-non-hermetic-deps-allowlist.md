# Bazel fuchsia package non hermetic deps allowlist

- **Learned from:** human review on CL Ie9d651577c24f541db3b76fe7c4d99578fa4d076, patchset 4 (task `fake-netstack`)
- **Date:** 2026-10-27
- **Changed:** `checks/bazel_minimality.sh`, `checks/migration_sanity.sh`, `prompts/coder.md`, `prompts/playbooks/case2_dual_build.md`

## Root cause

When a migrated fx_package is wrapped with bazel_fuchsia_package in BUILD.gn
for external non-assembly GN dependents, prebuilt_package adds an implicit
dependency on //build:non_hermetic_deps. Neither prompts/coder.md nor
prompts/playbooks/case2_dual_build.md instructed the coding agent to add
"//<dir>:<pkg>" and "//<dir>:<pkg>.package" to the prebuilt_package visibility
allowlist in group("non_hermetic_deps") in //build/BUILD.gn, migration_sanity.sh
did not verify that bazel_fuchsia_package targets are allowlisted in
//build/BUILD.gn, and bazel_minimality.sh did not permit //build/BUILD.gn
allowlist edits as part of a package migration.

## Why not a one-off fix

Updating only the single target directory's BUILD.gn and //build/BUILD.gn would
leave future migrations wrapping fx_package with bazel_fuchsia_package failing
GN generation on //build:non_hermetic_deps visibility or rejected by
checks/bazel_minimality.sh when updating build/BUILD.gn.
