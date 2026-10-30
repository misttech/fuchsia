# Diffengine binary discovery and proc race resilience in static checks

- **Learned from:** inner loop friction on a local task (task `pkg-isolated-ota-env`)
- **Date:** 2026-10-29
- **Changed:** `checks/gn_attr_parity.sh`, `checks/migration_sanity.sh`, `checks/visibility_audit.sh`, `tools/run_checks.sh`

## Root cause

Static checks and the check runner maintained divergent ad-hoc helpers for
locating the planter executable used by `planter diff --local --check-findings`:
`gn_attr_parity` previously lacked a defined `find_planter_bin` helper when
`collect_diffengine_alignment_findings` invoked it, while `/proc/<pid>/exe`
scanning across `gn_attr_parity`, `migration_sanity`, `visibility_audit`, and
`run_checks` either failed to filter out short-lived sibling `planter diff`
subprocesses via `/proc/<pid>/cmdline`, fell through to dead `/proc/<pid>/exe`
paths on `os.readlink` races, or deduplicated transient `/proc/<pid>/exe` inodes
in a way that shadowed the stable on-disk binary and running daemon.

## Why not a one-off fix

Re-running or patching a single migration task cannot prevent `NameError`
crashes, `FileNotFoundError` `/proc/<pid>/exe` races, or dropped `review_diff`
alignment coverage across concurrent migrations; all check and runner scripts
invoking `planter diff --local --check-findings` (`checks/gn_attr_parity.sh`,
`checks/migration_sanity.sh`, `checks/visibility_audit.sh`, and
`tools/run_checks.sh`) must share consistent, race-free
`find_planter_bins`/`find_planter_bin` discovery that filters transient `diff`
subprocesses, resolves stable on-disk readlink targets, and falls back across
candidate binaries.
