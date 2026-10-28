# Dual-build documentation and C++ dependency edge-kind parity: comments for bazel2gn override directives, and public header dependencies kept in deps (GN public deps)

- **Learned from:** human review on CL I48ab730b0a62b1a5713c7c485dc65154b6b9e4ee, patchset 2 (task `virt-libs`)
- **Date:** 2026-10-27
- **Changed:** `checks/cc_header_deps_and_annotations.sh`, `checks/manifest.json`

## Root cause

No check looked for a human comment next to non-test # @bazel2gn: directives:
migration_sanity only does this for test skips, and bazel_minimality only for
path_overwrite filegroups. No check read the #includes in hdrs files to confirm
that their dependencies are in deps (GN public_deps) and not implementation_deps
(GN deps). gn_dep_parity compares labels against GN, and GN parity alone does
not show that the header exposes the dependency. Both rules lived only in
reviewer learnings.

## Why not a one-off fix

Every C/C++ library migration can put a dependency used by a header in
implementation_deps. Every dual-build migration can add raw_attribute,
raw_overwrite, path_overwrite, value_overwrite or skip directives without saying
why. Fixing guest_config and virtio_block_test_util by hand leaves the next task
to repeat the mistake and lean on human reviewers again. A check run on every
task catches it before review.
