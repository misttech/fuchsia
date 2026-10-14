# File local gn template target type resolution

- **Learned from:** inner loop friction on a local task (task `pkg-update-installer-ext`)
- **Date:** 2026-10-02
- **Changed:** `checks/gn_target_type_parity.sh`, `checks/manifest.json`

## Root cause

gn_target_type_parity recorded the raw identifier of each top-level target
invocation in BUILD.gn without resolving file-local single-target template()
definitions that factor common attributes over target_name (such as sharing
sources and dependencies across regular and fdomain or test variants). When
bazel2gn expands the migrated targets into their underlying GN rules (e.g.
rustc_library, source_set, static_library), the check falsely reported a target
type change from the file-local template name to the underlying rule, and in
full-removal migrations failed to recognize when a file-local template expanded
to source_set(target_name) or static_library(target_name) for alwayslink parity.

## Why not a one-off fix

Many Fuchsia packages define file-local template() helpers in BUILD.gn to share
common attributes across target variants (such as regular and fdomain Rust
libraries or C++ source_set/static_library variants). Resolving file-local
single-target template() wrappers to the underlying target template(s) they
invoke on target_name in gn_target_type_parity handles all such packages across
the repository while still enforcing source_set/static_library alwayslink parity
and flagging unexpanded multi-target or imported .gni templates.
