# Dual build visibility overwrite and unmodified gn sentinel suppression

- **Learned from:** inner loop friction on a local task (task `pkg-stash`)
- **Date:** 2026-10-19
- **Changed:** `checks/migration_sanity.sh`, `prompts/reviewers/dual_build_sentinel.md`, `prompts/reviewers/target_parity.md`

## Root cause

When a migration widens Bazel visibility on an already-migrated dependency whose
BUILD.gn exposes the target via a pre-sentinel forwarding group() and preserves
GN visibility = [ ":*" ] via # @bazel2gn:raw_overwrite, running fx bazel2gn
leaves the dependency's BUILD.gn unchanged in git. review_diff (diffengine)
flags gn_unmodified on any package where BUILD.bazel is modified in the CL
without a corresponding BUILD.gn diff, and migration_sanity only passed filtered
candidate_dirs to its diffengine pre-population check while reviewer prompts
still demanded //visibility:public instead of //:__subpackages__ on
globally-used libraries.

## Why not a one-off fix

Fixing only a single package directory leaves migration_sanity unable to
suppress false-positive review_diff gn_unmodified findings across all changed
BUILD.bazel packages whose BUILD.gn already contains a synced BAZEL2GN SENTINEL,
and leaves the reviewer prompts contradicting visibility_audit and find_rdeps
whenever any globally-used dependency is widened to ["//:__subpackages__"] with
# @bazel2gn:raw_overwrite:[ ":*" ].
