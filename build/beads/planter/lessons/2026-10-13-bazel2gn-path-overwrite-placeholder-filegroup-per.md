# Bazel2gn path overwrite placeholder filegroup per dep

- **Learned from:** human review on CL If2ba29d6afa754cc8f0a5b0f91c004150f381a21, patchset 7 (task `dmlconfig`)
- **Date:** 2026-10-13
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

The previous path_overwrite check in checks/bazel_minimality.sh wrongly required
combining multiple unmigrated GN-only dependencies into a single generic
`filegroup(name = "bazel2gn_placeholder")` entry via quote injection and
exempted same-package `fidl_library` targets used as stand-ins for missing
`*_rust_next` bindings, whereas human reviewers require one `# @bazel2gn:skip`
`filegroup` per overwritten path with a unique name derived from the overwritten
path.

## Why not a one-off fix

Fixing only a single package's BUILD.bazel leaves checks/bazel_minimality.sh
enforcing the opposite anti-pattern across future migrations whenever a
dual-build target has multiple unmigrated GN-only dependencies or missing FIDL
language-binding sub-targets in a `select()` branch.
