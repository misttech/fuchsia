# C/C++ migrations: Fuchsia-only constraints, header deps, and dead GCC selects

- **Learned from:** human review of CL 1917815 (`//sdk/lib/driver/node/cpp`),
  CL 1924006 (`//sdk/lib/fuchsia-mem-ext`), CL 1918746
  (`//zircon/system/utest/core/*`) and CL 1915654 (`//src/ui/scenic/lib/shutdown`)
- **Date:** 2026-10-28
- **Changed:** `checks/migration_sanity.sh`, `prompts/coder.md`

## Root cause

Three recurring C/C++ mistakes that build fine in the CL's own presubmit:

- Libraries whose sources use `zx::vmo`, `<zircon/syscalls.h>` or FIDL messaging
  without an `#ifdef __Fuchsia__` guard had no
  `target_compatible_with = ["@platforms//os:fuchsia"]`, or had it on only some
  sibling packages of the same change. They only fail once a host target reaches
  them.
- A header in `hdrs` included `<lib/async/cpp/executor.h>` while
  `//sdk/lib/async:async-cpp` was in `implementation_deps` (copied from GN
  `deps`). The library compiles, but its dependents don't get the include path.
- A `select()` on `@rules_cc//cc/compiler:gcc` was copied from a GN
  `is_gcc` branch. Fuchsia has no GCC toolchain in Bazel, so it never matches.

## Remediation

1. `migration_sanity.sh` warns on a new `@rules_cc//cc/compiler:gcc` branch
   (`gcc_compiler_select`).
2. `prompts/coder.md` covers Fuchsia-only `target_compatible_with` and putting
   dependencies that `hdrs` include in `deps`. Both need reading the sources,
   so they stay prompt guidance.
