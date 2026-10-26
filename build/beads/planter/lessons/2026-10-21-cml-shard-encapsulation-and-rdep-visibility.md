# Cml shard encapsulation and rdep visibility tiering

- **Learned from:** human review on CL I4eb1208833ce04e0f6df1ff341fbc538e588d2cc, patchset 2 (task `pkg-http-client`)
- **Date:** 2026-10-21
- **Changed:** `checks/bazel_minimality.sh`, `checks/visibility_audit.sh`, `prompts/coder.md`, `tools/find_rdeps.sh`

## Root cause

First, `prompts/coder.md` and `checks/bazel_minimality.sh` instructed or
permitted exporting `.shard.cml` files via `exports_files(...)` and listing
unrolled transitive `.shard.cml` labels in `fx_component_manifest(includes =
[...])` instead of defining `cml_shard` targets with transitive `includes`.
Second, `checks/visibility_audit.sh` and `tools/find_rdeps.sh` excluded
`BUILD.gn` callers of a pre-sentinel `group("<name>")` even when `BUILD.bazel`
defined a corresponding `alias(name = "<name>")` target (undercounting tree-wide
callers of wrapper group/alias targets), merged public alias rdeps into internal
implementation targets with restricted GN visibility, disallowed broad
visibility on `cml_shard`/`expect_includes` targets consumed via `.cml`
includes, and jumped directly to `//:__subpackages__` for medium-scale libraries
(< 50 callers) where collapsing `//src/...` callers into `//src:__subpackages__`
suffices.

## Why not a one-off fix

Fixing only a single package leaves `find_rdeps.sh`, `visibility_audit.sh`,
`bazel_minimality.sh`, and `prompts/coder.md` miscomputing reverse-dependency
visibility tiers for pre-sentinel `group`/`alias` pairs, medium-scale multi-area
libraries, and `cml_shard` targets across all future migrations.
