# `idk_cc_*` targets don't list `//sdk:__pkg__` in `visibility`

- **Learned from:** human review of CL 1917815 (`//sdk/lib/driver/node/cpp`) and
  CL 1924006 (`//sdk/lib/virtgralloc`)
- **Date:** 2026-10-28
- **Changed:** `checks/visibility_audit.sh`, `prompts/coder.md`

## Root cause

`owning_bazel_target()` maps any `<name>_<suffix>` reference to `<name>`, so the
`<name>_sdk` atom in `//sdk/BUILD.gn` and the `<name>_idk` atom in
`//sdk/atom_lists.bzl` looked like callers in `//sdk`, and
`uncovered_rdep_visibility` asked the coder to add `"//sdk:__pkg__"`. Neither
build needs it: `idk_cc_*` macros append `//sdk:__pkg__` to the target and its
`_idk` atom, and GN `sdk_*` templates only forward `visibility` to the library,
not to the `<name>_sdk` atom.

## Remediation

1. `visibility_audit.sh` ignores references to `_sdk` / `_idk` atoms of an
   `idk_*` target when collecting callers.
2. It reports an explicit `"//sdk:__pkg__"` on an `idk_*` target as
   `redundant_idk_sdk_visibility` unless something in `//sdk` depends on the
   library target itself.
3. `prompts/coder.md` says not to list it.
