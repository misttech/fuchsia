# File local gn template inner target name expansion parity

- **Learned from:** inner loop friction on a local task (task `src-sys-run-test-suite`)
- **Date:** 2026-10-27
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

When a base BUILD.gn defines a file-local template("<tmpl>") wrapping an inner
rule such as rustc_library(target_name), diffengine parses both the outer
<tmpl>("<name>") invocations and the inner <rule>(target_name) block as separate
GN targets in the base BUILD.gn. When all concrete invocations are migrated to
BUILD.bazel and the local template definition is removed,
collect_local_template_and_sentinel_targets in checks/gn_attr_parity.sh recorded
only the outer concrete target names in expanded_tmpl_targets and omitted the
inner target_name token from the template body, leaving diffengine's
unmatched_target error on GN target "target_name" uncovered.

## Why not a one-off fix

Hardcoding specific package paths or template names would fail on any other
Fuchsia package that uses a file-local GN template (for example to share
deps/sources across Fuchsia, host, or FDomain variants) and expands those
invocations into explicit Bazel targets during migration.
