# Test footer validation vs unconfigured tests json

- **Learned from:** inner loop friction on CL I7938a093b025d82103ee81db30fab26c7b4ba636, patchset 1 (task `src-developer-ffx-lib-mdns-discovery`)
- **Date:** 2026-10-23
- **Changed:** `checks/build_verification.sh`, `checks/commit_message_format.sh`, `prompts/coder.md`

## Root cause

The `unconfigured_fx_test_footer_label` check in `checks/build_verification.sh`
required `fx test` commit footer labels to appear in `<build dir>/tests.json`
(and rejected all `fx test` footers when `<build dir>/tests.json` had 0 entries
because `args.gn` was configured without `--with-test`), directly contradicting
the prompt and `commit_message_format.sh` guidance to list `fx test
//<dir>:<test_target>` for migrated tests exported by `bazel_test_suite` and
causing the coder to report `machinery_conflict` in `disputed_findings`.

## Why not a one-off fix

Any Fuchsia GN-to-Bazel migration run in a default build directory without
`--with-test` in `args.gn` has an empty or partial `<build dir>/tests.json`, so
validating `Test: fx test //<dir>:<test_target>` footers must check whether the
target is a leaf test target exported by `bazel_test_suite` or defined as a test
rule in `BUILD.bazel`/`BUILD.gn` (while still rejecting GN `group("tests")`
`:tests`, `bazel_test_suite` wrappers, and non-test library targets) rather than
requiring presence in `<build dir>/tests.json`.
