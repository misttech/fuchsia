# Allow Bazel 8 symbolic-macro package grants and consolidated forwarding-group attr parity

- **Date:** 2026-10-29
- **Changed:** `checks/visibility_audit.sh`, `checks/gn_attr_parity.sh`

## Root cause

1. Bazel 8 symbolic macros (`macro(...)`, such as `idk_cc_shared_library` in
   `//build/bazel/rules/idk/private`) evaluate visibility of labels constructed
   from `attr.string` attributes (such as `version_script`) against the macro's
   definition package rather than the caller package, requiring explicit
   `//<macro_def_pkg>:__pkg__` visibility on same-package helper targets that
   `visibility_audit.sh` previously flagged as `false_positive_rdep_visibility`.
2. When a pre-migration forwarding `group("<dir>") { public_deps = [":<lib>"] }`
   and zero-rdep `<tmpl>("<lib>")` are consolidated into `<tmpl>("<dir>")` with
   identical sources and preserved GN-only attributes (`@bazel2gn:raw_attribute`),
   `gn_attr_parity.sh` skipped `"<dir>"` because `"<dir>"` existed as a `group`
   in `before_targets`, firing a false `gn_only_attr_dropped` error.

## Why not a one-off fix

Both Bazel 8 symbolic macros in `//build/bazel/rules/idk/private` and legacy GN
`group("<dir>") -> <lib>` forwarding wrappers recur across SDK and system
migrations.
