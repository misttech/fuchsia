# `public_configs` on `fx_cc_*` only reaches GN

- **Learned from:** review of CL 1927350 (`//src/lib/ubsan-custom`)
- **Date:** 2026-10-28
- **Changed:** `prompts/coder.md`

## Root cause

GN `source_set("handlers")` in `//src/lib/ubsan-custom` is header-only and uses
`public_configs = [ ":no-ubsan" ]` so the single `.cc` file that includes it is
compiled without UBSan; otherwise a UBSan failure inside the handlers recurses.
The migration kept `public_configs = [":no-ubsan"]` on
`fx_cc_library(name = "handlers")`, but `fx_cc_library` ignores `public_configs`
(and `configs`) in Bazel and only passes them to bazel2gn. GN dependents keep the
flags; a future Bazel dependent compiles the handlers instrumented, with nothing
in BUILD.bazel saying it must set `build_flags = [":no-ubsan"]` itself.

Detecting which configs change dependents' semantics would need flag-level
knowledge, so this stays prompt guidance.

## Remediation

`prompts/coder.md` says `public_configs`/`configs` on `fx_cc_*` are GN-only, and
asks the coder to document on the target which `build_flags()` Bazel dependents
must set when GN relied on `public_configs` to change how they compile.
