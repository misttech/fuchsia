# Determining bazel visibility subsystem rollup

- **Learned from:** reviewer feedback on CL Icfa2172f56ca244e5e14b46bf277983fa0e84fc6, patchset 2 (task `pkg-fidl-power-battery`)
- **Date:** 2026-10-13
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

`visibility_audit.sh` and `find_rdeps.sh` previously recommended
`["//visibility:public"]` for globally used targets, only allowed subsystem
rollup entries when narrow visibility exceeded 15 entries, and did not share
per-target reverse dependencies between a base target (`<t>`) and sibling
suffixed targets (`<t>_<suffix>`) in the same package. This caused migrations to
emit either forbidden `//visibility:public`, overly granular leaf-package
visibility lists, or `# @bazel2gn:raw_overwrite:` workarounds instead of the
subsystem-grouped visibility (`<= 15` entries, or `["//:__subpackages__"]` for
foundational libraries) specified in `determining_bazel_visibility/SKILL.md`.

## Why not a one-off fix

Every migrated target with reverse dependencies across multiple subsystems must
follow `determining_bazel_visibility/SKILL.md`: never use
`"//visibility:public"` on in-tree targets, group visibility by 2-3 level
subsystem prefixes when leaf package lists would be unwieldy, use
`["//:__subpackages__"]` when subsystem grouping still exceeds 15 entries, and
allow base targets and their sibling suffixed wrapper targets in the same
package to share visibility without `# @bazel2gn:raw_overwrite:`.
