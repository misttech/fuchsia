# Dual build diffengine mispaired target line alignment

- **Learned from:** inner loop friction on CL I3c18384211358d98607eb6bdc50eb4dd1b8df163, patchset 2 (task `swd-pkg-system-update-checker`)
- **Date:** 2026-10-23
- **Changed:** `checks/migration_sanity.sh`

## Root cause

In checks/migration_sanity.sh, parse_gn_text() matched GN target declarations
using `^\s*([a-zA-Z0-9_]+)\(` with `re.M` and computed 1-based line numbers at
`tm.start()`. Because `\s` matches newlines under `re.M`, any GN target preceded
by a blank line had its line number computed on the blank line (off by one, e.g.
140 instead of 141 in the base BUILD.gn and 49 instead of 50 in the current
BUILD.gn). Consequently, the INFO `unmatched_target` compatibility findings
emitted by `record_mispaired_crate_name_target_compat_info` (when diffengine
greedily pairs a kept pre-sentinel GN package target with a Bazel binary/library
via normalized `crate_name`, leaving the same-named GN target unmatched) failed
to match the exact line number reported by `review_diff` in planter's gate.

## Why not a one-off fix

Hardcoding a line number offset or target-specific suppression for a single
package would fail on any other dual-build package where a pre-sentinel GN
target shares a normalized name with a Bazel
`crate_name`/`output_name`/`binary_name` and the displaced GN target is preceded
by one or more blank lines across base or current revisions. Fixing
`parse_gn_text` to match only horizontal leading indentation (`^[ \t]*`) and
measure line numbers at `tm.start(1)`, while collecting base target locations
across candidate base revisions, ensures exact line-number parity systemically
across all packages.
