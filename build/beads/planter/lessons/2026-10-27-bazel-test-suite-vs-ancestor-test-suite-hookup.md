# Bazel test suite vs ancestor test suite hookup

- **Learned from:** human review on CL I9de97750a71785edfbe34079820cee020955047a, patchset 1 (task `netstack3-udp`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

checks/cq_reachability.sh enforced wiring into an ancestor BUILD.bazel
test_suite when ancestor_suite_covers_subtree returned True, but had two blind
spots: (1) ancestor_suite_covers_subtree read the working-tree BUILD.bazel
without excluding newly added target_dirs entries absent at
$PLANTER_CHANGE_BASE, allowing an agent's own edit to bootstrap coverage on an
empty or unrelated ancestor test_suite, and (2) when
ancestor_suite_covers_subtree returned False (because sibling packages in the
subtree export tests via per-package bazel_test_suite in BUILD.gn or the
ancestor test_suite only aggregates a different child subsystem),
cq_reachability.sh did not reject newly hooking the package's tests into the
ancestor BUILD.bazel test_suite.

## Why not a one-off fix

Fixing only a single package leaves every future migration under a subsystem
whose siblings use per-package bazel_test_suite in BUILD.gn (or beneath a
top-level umbrella BUILD.bazel test_suite that only aggregates other child
subtrees) free to hook leaf tests into the ancestor BUILD.bazel test_suite
without any static check error.
