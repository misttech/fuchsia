# Dual build skipped target rdep filtering and test footer validation

- **Learned from:** inner loop friction on a local task (task `swd-pkg-system-update-committer`)
- **Date:** 2026-10-23
- **Changed:** `checks/visibility_audit.sh`, `prompts/coder.md`, `tools/find_rdeps.sh`

## Root cause

In dual-build packages where non-package BUILD.bazel targets are marked with #
@bazel2gn:skip (or where external .gn/.gni files reference target names absent
from BUILD.bazel), compute_package_rdeps in visibility_audit.sh and
find_rdeps.sh added .gn/.gni callers to true_rdep_packages before verifying that
the referenced target is a GN-synced Bazel target, and visibility_audit.sh
initially referenced bazel2gn_skipped_targets without propagating skipped
non-package targets into non_gn_synced_targets. In addition, coder.md did not
explicitly forbid listing fx test <target> in CoderReport.test_footers when the
target is absent from <build dir>/tests.json (fx test --dry fails).

## Why not a one-off fix

Hardcoding exemptions for a single package directory would leave every other
dual-build migration that uses # @bazel2gn:skip targets or has stale/GN-only
sub-target references in external BUILD.gn files vulnerable to false-positive
rdep visibility recommendations and unconfigured fx test footers.
