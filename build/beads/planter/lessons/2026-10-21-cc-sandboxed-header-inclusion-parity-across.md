# Cc sandboxed header inclusion parity across preprocessor branches

- **Learned from:** revert on a local task (task `pkg-update-package-tests`)
- **Date:** 2026-10-21
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

GN compiles C/C++ targets unsandboxed against the source tree, allowing
same-directory quoted #include directives to resolve even when the header is
omitted from target sources, whereas Bazel enforces strict sandboxed header
inclusion checking. When an undeclared local #include is inside a non-default
CPU preprocessor branch (such as riscv64), single-CPU x64 Bazel build
verification does not preprocess that branch, allowing missing headers in
migrated C/C++ or third_party/rust_crates/compat targets to pass local builds
and fail on non-default-CPU CI builders.

## Why not a one-off fix

Fixing only a single C/C++ compat or library target leaves future migrations
vulnerable whenever a migrated C/C++ target or a referenced
third_party/rust_crates/compat C/C++ library has an undeclared same-directory
header inside an architecture-specific #if branch; scanning all C/C++ source and
header files referenced by BUILD.bazel across all preprocessor branches catches
missing local headers deterministically regardless of host/target CPU.
