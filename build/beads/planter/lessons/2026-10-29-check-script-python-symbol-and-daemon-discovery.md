# Check script python symbol and daemon discovery hardening

- **Learned from:** inner loop friction on a local task (task `pkg-tests-null-update-checker`)
- **Date:** 2026-10-29
- **Changed:** `checks/build_only_scope.sh`, `checks/gn_attr_parity.sh`, `tools/run_checks.sh`

## Root cause

Embedded Python check helpers copied across check scripts referenced undefined
helper symbols (find_planter_bin in gn_attr_parity when reconciling diffengine
alignment findings, and target_dir in build_only_scope when PLANTER_TARGET_DIRS
is unset), and scanned /proc in arbitrary order without prioritizing ancestor
daemon PIDs or resolving stable on-disk readlink targets.

## Why not a one-off fix

Check script NameError crashes and transient /proc/<pid>/exe races occur inside
shared static check harnesses regardless of which Fuchsia package is being
migrated, blocking every task that invokes run_checks.sh or standalone static
check execution until hardened across the shared check and runner scripts.
