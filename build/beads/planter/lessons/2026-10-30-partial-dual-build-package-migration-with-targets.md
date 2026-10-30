# Partial dual-build package migration with targets blocked by unmigrated external GN-only dependencies

- **Learned from:** human review on CL I4487358a1c002cb344d0733a0d8dc56a22fe7106, patchset 2 (task `src_graphics_display_lib_driver-framework-migration-utils_logging`)
- **Date:** 2026-10-30
- **Changed:** `prompts/reviewers/dual_build_sentinel.md`

## Root cause

While migration_sanity allows retaining convertible GN targets above the
BAZEL2GN SENTINEL when their external dependencies are not yet migrated to Bazel
(emitting an authoritative INFO unmatched_target finding) and bazel_minimality
rejects sweeping cross-area dependencies into the CL, the dual_build_sentinel
reviewer prompt still instructed the reviewer to reject any BUILD.gn with a
sentinel that retains a convertible non-test target above the sentinel, causing
the coder and reviewer to conflict on partial package migrations.

## Why not a one-off fix

Fixing only a single package leaves the dual_build_sentinel reviewer prompt in
conflict with migration_sanity and scoping_changelists.md across every future
partial dual-build migration where some targets in a BUILD.gn have Bazel-ready
dependencies while sibling targets in the same BUILD.gn remain blocked on
unmigrated cross-area GN dependencies.
