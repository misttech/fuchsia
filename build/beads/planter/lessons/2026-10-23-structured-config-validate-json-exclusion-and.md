# Structured config validate json exclusion and unmerged parent dep gating

- **Learned from:** inner loop friction on a local task (task `src_sys_time_httpsdate_time_source`)
- **Date:** 2026-10-23
- **Changed:** `checks/build_verification.sh`, `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

First, validate_json_exclusions in build_verification.sh matched direct
fidl_library and validate_json calls when excluding _validate_json_action
targets from top-level :all builds, but missed
fuchsia_structured_config_rust_lib, which internally instantiates
fidl_library(name = '<name>_fidl_internal') and generates
'<name>_fidl_internal_validate_ir_json', causing assert_no_deps_aspect to crash
with TypeError on its scalar data attribute. Second,
check_unmerged_parent_bazel_package_deps in cq_reachability.sh ran even when
PLANTER_CHANGE_BASE=HEAD (before the task commit exists, when HEAD is the base
commit that parity checks diff against) and flagged any package where git
cat-file -t origin/main:<pkg>/BUILD.bazel was not a blob without verifying that
<pkg>/BUILD.bazel was actually touched in stack_base..HEAD~1, falsely flagging
git submodule packages whose root-repo tree entry is a gitlink.

## Why not a one-off fix

Fixing only a single package directory leaves every future migration using
fuchsia_structured_config_rust_lib failing in build_verification :all builds due
to assert_no_deps_aspect crashing on <name>_fidl_internal_validate_ir_json, and
leaves every stacked or submodule-consuming migration blocked by false
unmerged_parent_bazel_package_dep errors during pre-commit inner-loop checks.
