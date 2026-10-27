# Unrelated upstream python mypy ninja build failure classification

- **Learned from:** cq failure on CL I77d7ab703a21e180fc93d4de8c42f5878f0edd5d, patchset 1 (task `src-lib-network-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/build_verification.sh`, `checks/manifest.json`

## Root cause

An upstream commit on origin/main added an import of bazel_test_packages to
build/bazel/scripts/bazel_tests_utils.py and exported Bazel test packages into
<ninja_build_dir>/bazel_test_packages/ without adding bazel_test_packages.py to
//build:build_tests_json_tests in //build/BUILD.gn, causing
host_x64/obj/build/build_tests_json_tests_mypy.mypy_checked to fail whenever the
exported directory exists in the Ninja build directory. In
checks/build_verification.sh, unrelated_gn_ninja_failure only recognized
Rust/Clippy Ninja actions via --transdeps and did not match Ninja status lines
with active-process counts or Python mypy/build-time-test actions
(--library_infos / .mypy_checked).

## Why not a one-off fix

Ignoring or working around the failure in a single migrated package is
insufficient because any GN-to-Bazel migration task running a full GN build or
recovering from an upstream CQ failure will hit the same unrelated Python
mypy/build-time-test Ninja failure and must classify it deterministically via
unrelated_gn_ninja_failure in build_verification.sh without touching unrelated
//build/ files.
