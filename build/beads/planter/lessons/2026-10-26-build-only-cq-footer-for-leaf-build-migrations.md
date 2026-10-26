# Accelerating CQ for leaf build migrations using Build-Only: true

- **Learned from:** infrastructure change fxr/1913427 (Bug 576757166) allowing
  Build-Only footers on pure BUILD.gn/BUILD.bazel [bazel_migration] CLs
- **Date:** 2026-10-26
- **Changed:** `checks/commit_message_format.sh`, `prompts/coder.md`

## Root cause

Fuchsia presubmit CQ runs device and emulator tests on every CL, taking 45-90
minutes per run and creating heavy load on infra test bots. An infra analysis of
~600 migration CLs over 90 days revealed that only ~1.2% had test failures (99%
of true issues were caught during compilation and shac static checks).

In fxr/1913427, Fuchsia infrastructure relaxed the `No-Build-Only-Footer` submit
requirement specifically for `[bazel_migration]` CLs that touch `BUILD.gn` and
`BUILD.bazel` exclusively, permitting `Build-Only: true` on `Commit-Queue+2`.

## Remediation

Planter machinery now recognizes, validates, and supports `Build-Only: true` for
qualifying leaf migrations:

1. **Eligibility criteria**:
   - The commit subject starts with `[bazel_migration]`.
   - The change modifies `BUILD.gn` and `BUILD.bazel` files exclusively.
   - The change does not touch Assembly Input Bundles (`bundles/assembly/`,
     `build/images/`) or alter product contents, even transitively.
2. **Validation**:
   `checks/commit_message_format.sh` enforces the Gerrit submit requirement by
   blocking `Build-Only: true` if any non-BUILD file or AIB file is modified,
   and reports INFO when an eligible CL can benefit from it.
3. **Turnaround impact**:
   Build-only CQ runs finish in 10-15 minutes instead of 45-90 minutes,
   accelerating individual CQ runs by 3x-5x without compromising compilation
   or dual-build parity verification.
