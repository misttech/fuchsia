# Dual build host test bazel2gn skip parity

- **Learned from:** inner loop friction on a local task (task `netstack3-macros`)
- **Date:** 2026-10-16
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`, `prompts/coder.md`, `prompts/playbooks/case2_dual_build.md`

## Root cause

prompts/coder.md, prompts/playbooks/case2_dual_build.md, and
checks/migration_sanity.sh previously instructed coders that host_*_test rules
needed no # @bazel2gn:skip annotation in dual-build packages because bazel2gn
maps them to __NO_GN_EQUIVALENT__. However, review_diff treats any un-skipped
target in a dual-build BUILD.bazel as a synced target expected below the
BAZEL2GN SENTINEL and matches it by name against the hand-written
bazel_test_suite above the sentinel, failing with sentinel_drift, two_case_rule,
and attribute_alignment unless # @bazel2gn:skip is present.

## Why not a one-off fix

Adding # @bazel2gn:skip in a single package leaves the contradictory instruction
in prompts/coder.md and prompts/playbooks/case2_dual_build.md intact, causing
every future dual-build migration with a host_*_test or wrap_host_rust_test
target to hit the same review_diff errors and machinery_conflict.
