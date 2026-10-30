# Compiled action bazel2gn attribute and location semantics

- **Learned from:** human review on [CL 1932207](https://fuchsia-review.googlesource.com/c/fuchsia/+/1932207), patchset 1 (task `b2gn-compiled-action`)
- **Date:** 2026-10-30
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

The static checks audited genrule variable substitutions and Rust/C++ target
attributes, but had no checks for compiled_action() location expansion scope
(restricting $(location)/$(execpath) to srcs and outs or path-overwritten deps
rather than un-annotated deps), GN-only depfile string semantics (using
$(RULEDIR)/<depfile> in args rather than declaring depfile as a Bazel output),
or explicit tool_output_name specification without bazel2gn whole-file
inference.

## Why not a one-off fix

Fixing only a single compiled_action target or a single CL leaves future
migrations and rule/converter updates free to pass un-annotated deps or depfile
labels to $(location ...), declare unused Bazel depfile outputs, or rely on
implicit tool_output_name inference instead of setting tool_output_name
explicitly in BUILD.bazel.
