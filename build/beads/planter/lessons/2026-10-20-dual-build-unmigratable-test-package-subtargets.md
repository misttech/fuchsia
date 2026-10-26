# Dual build unmigratable test package subtargets and disputed findings gating

- **Learned from:** inner loop friction on a local task (task `pkg-dns-lib`)
- **Date:** 2026-10-20
- **Changed:** `prompts/coder.md`

## Root cause

prompts/coder.md instructed the coder to report contradictory instructions as
machinery_conflict in disputed_findings without explicitly stating that when a
fuchsia_test_package must remain in BUILD.gn above the sentinel (e.g., due to
GN-only component dependencies), its rustc_test and fx_component_manifest
sub-targets must also remain in BUILD.gn even if named in the task description,
and that disputed_findings must be empty whenever run_checks.sh exits 0.

## Why not a one-off fix

Any dual-build task description generated before target inspection may list a
rustc_test or component manifest sub-target alongside a GN-only test package;
clarifying both the sub-target retention rule and the empty disputed_findings
requirement when run_checks.sh passes prevents false machinery_conflict loops
across all future dual-build migrations.
