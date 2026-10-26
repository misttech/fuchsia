# Coder report disputed findings and test footer alignment

- **Learned from:** inner loop friction on CL I4b835ab2d696fc7a345394547335937da73039a8, patchset 1 (task `src-developer-ffx-lib-discovery`)
- **Date:** 2026-10-23
- **Changed:** `checks/build_verification.sh`, `prompts/coder.md`, `tools/run_checks.sh`

## Root cause

prompts/coder.md instructed the coder to set disputed_findings to [] unless
remediation_validation requires file edits when review comments only target
/COMMIT_MSG, and build_verification emitted a bazel_tests_note warning against
recording fx test commands when <build dir>/tests.json had no entries for the
package's bazel_test_suite. Because Planter converts any non-empty
CoderReport.disputed_findings array into blocking machinery_conflict ERROR
findings, the coder preemptively populated disputed_findings with
remediation_validation and build_verification entries even after run_checks.sh
exited 0 with 0 blocking findings.

## Why not a one-off fix

Fixing only a single package directory would leave the misleading
remediation_validation exception in prompts/coder.md and the conflicting
bazel_tests_note wording in checks/build_verification.sh, causing future
stacked-CL rebases and host-test migrations to fail with self-inflicted
machinery_conflict errors after passing all checks.
