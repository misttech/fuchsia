# visibility_audit attributes `.gni` template callers to the invoking BUILD.gn

- **Learned from:** human review of CL 1926390
  (`//src/security/tests/pkg_test/poison_tuf_signature`)
- **Date:** 2026-10-28
- **Changed:** `checks/visibility_audit.sh`, `prompts/coder.md`

## Root cause

The script counted a label written inside a template in `//a/b/foo.gni` as a
caller in `//a/b`. GN resolves and visibility-checks labels in a template in the
BUILD.gn that invokes the template. In CL 1926390 `bad_signature_update.gni` (in
`tests/bad_signature_update/`) runs `poison_tuf_signature`, but `tests/BUILD.gn`
instantiates it, so the coder granted visibility to a package that never depends
on the tool.

## Remediation

1. References in `.gni` files outside `//build` are attributed to the packages
   whose BUILD.gn imports the `.gni` (following `.gni` imports up to 3 levels),
   and those packages count as direct GN callers for `uncovered_rdep_visibility`.
2. `prompts/coder.md` says to grant visibility to the package that invokes a
   template, not the `.gni`'s directory.
