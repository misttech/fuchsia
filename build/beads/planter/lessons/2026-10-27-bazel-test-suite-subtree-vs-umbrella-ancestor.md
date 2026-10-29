# Bazel test suite subtree vs umbrella ancestor scoping

- **Learned from:** inner loop friction on CL I47f520f73c99d58f1d50ef3b4ce6b630401bc506, patchset 1 (task `netstack3-echo`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`, `prompts/coder.md`

## Root cause

checks/cq_reachability.sh (nearest_ancestor_test_suite and
preferred_ancestor_area_suite) matched any ancestor BUILD.bazel test_suite in
the directory path—even when that ancestor was a higher-level umbrella suite
that only aggregated a different child subsystem (or had an empty tests list)
and sibling packages under the same parent directory already exported their
Bazel tests via per-package bazel_test_suite targets in BUILD.gn with
package-private test targets in BUILD.bazel.

## Why not a one-off fix

Exempting a single package directory would leave every other package in subtrees
that use per-package bazel_test_suite targets (while sitting beneath a
higher-level umbrella BUILD.bazel test_suite for another child subsystem)
blocked on false-positive bypassed_ancestor_test_suite errors.
