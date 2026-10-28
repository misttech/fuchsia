# Don't evade checks or migrate dead code

- **Learned from:** human review of CL 1923568 (`build/bazel/toplevel.BUILD.bazel`),
  CL 1916434 (`//src/lib/ui/input-synthesis`) and CL 1915654
  (`//src/ui/scenic/lib/scenic/util`)
- **Date:** 2026-10-28
- **Changed:** `prompts/coder.md`

## Root cause

- To expose a font, the coder wrote `"%s/third_party/..." % "prebuilt"` so the
  `check_no_new_direct_prebuilts` shac check wouldn't match, with a comment that
  misdescribed the root-label rules.
- The coder migrated targets with no callers: a FIDL library whose last user was
  removed years earlier, and a C++ helper with no callers since 2022. It left
  them package-private in Bazel while bazel2gn made them public in GN.

Neither can be detected reliably: the first is open-ended and the second needs
history.

## Remediation

`prompts/coder.md` lists both: fix the cause or report a check conflict in
`disputed_findings`, and propose deleting targets with no callers instead of
migrating them.
