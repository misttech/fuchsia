# Commit msg line length and ci test verification parity

- **Learned from:** human review on CL I67bd43ddde24e867bbc1015622e6952c9012c62f, patchset 3 (task `src_connectivity_overnet_lib_circuit`)
- **Date:** 2026-10-22
- **Changed:** `checks/commit_message_format.sh`, `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

First, `checks/commit_message_format.sh` did not enforce the
`scripts/shac/commit_msg_checker.py` line-length limits (subject line <= 65
characters and body lines <= 72 characters excluding URLs, indented code blocks,
and single-line footers), and `fx host-tool shac check` exits 0 on `commit_msg`
warnings so `build_verification.sh` did not block on them. Second,
`checks/cq_reachability.sh` only audited test targets already declared in the
new `BUILD.bazel`, so if a migration dropped `group("tests")` or
`with_unit_tests = true` from base `BUILD.gn`, or migrated a Rust test to
`fx_unittest_package` while omitting `wrap_host_rust_test` and
`:<name>_test($host_toolchain)` when base `BUILD.gn` ran the test on
`$host_toolchain`, no check flagged the missing CI test verification.

## Why not a one-off fix

Fixing only the single package's commit message wrapping and `bazel_test_suite`
/ `group("tests")` wiring leaves all future migrations vulnerable to uploading
commit messages with >72-character body lines that trigger Gerrit `shac`
`commit_msg` warnings and to dropping unit test or host-toolchain CI test
verification when migrating libraries that set `with_unit_tests = true` in base
`BUILD.gn`.
