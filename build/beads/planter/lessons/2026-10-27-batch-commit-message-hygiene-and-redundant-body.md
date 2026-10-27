# Batch commit message hygiene and redundant body boilerplate

- **Learned from:** cq failure on CL Iaa2e4e4a06b03bca4b85c475637090a69a5c8251, patchset 3 (task `src-connectivity-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/commit_message_format.sh`

## Root cause

The CQ failure in host_x64/obj/build/build_tests_json_tests_mypy.mypy_checked is
an unrelated upstream race condition in //build/BUILD.gn where
python_build_time_tests("build_tests_json_tests") omits
//build/bazel/scripts/bazel_test_packages.py from inputs/deps while
//build/bazel/target_tests creates a bazel_test_packages/ directory in the Ninja
working directory (already classified as preexisting_failure_gn_build by
build_verification.sh). However, while inspecting the batch commit message
alongside the newly added commit_subject_already_migrated_directory and
also_migrated_already_migrated checks, redundant_body_para_re in
checks/commit_message_format.sh only matched bare 'Migrate //<dir> [from GN] [to
Bazel]' paragraphs and missed redundant body paragraphs with trailing 'with
dual-build support via bazel2gn' boilerplate.

## Why not a one-off fix

Fixing only a single commit message in one batch task leaves
checks/commit_message_format.sh unable to flag 'Migrate //<dir> to Bazel with
dual-build support via bazel2gn' paragraphs in future dual-build migrations;
extending redundant_body_para_re in checks/commit_message_format.sh catches this
boilerplate pattern across all future migration commits.
