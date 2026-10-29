# Parallel migration outside changes dispute and gn foreach rdep discovery

- **Learned from:** inner loop friction on a local task (task `ffx-product-leaves-b`)
- **Date:** 2026-10-28
- **Changed:** `checks/visibility_audit.sh`, `prompts/coder.md`, `tools/find_rdeps.sh`

## Root cause

In parallel multi-directory migrations, sub-coders cannot edit shared files
outside their assigned slice directory and must request those edits in
outside_changes for the post-fanout integration coder to apply. Because
prompts/coder.md instructed coders to record any remaining blocking finding from
run_checks.sh in disputed_findings without excluding findings caused by
unapplied outside_changes, a parallel sub-coder listed unexported_bazel_test and
false_positive_rdep_visibility in disputed_findings, which caused the
orchestrator to abort the integration coder before applying outside_changes.
Additionally, find_rdeps.sh and visibility_audit.sh treated bare package strings
in GN list variables as references only to the default library target and
ignored foreach variable-interpolated target suffixes ("$var:<target>"), missing
GN callers of dual-built unit test targets.

## Why not a one-off fix

Multi-package leaf batches across the tree routinely share an ancestor
test_suite or parent BUILD.gn that aggregates sub-package libraries and unit
tests via GN foreach loops. Preventing parallel sub-coders from disputing
findings that await outside_changes and teaching find_rdeps.sh and
visibility_audit.sh to resolve foreach-interpolated target suffixes fixes this
class of friction across all parallel migrations.
