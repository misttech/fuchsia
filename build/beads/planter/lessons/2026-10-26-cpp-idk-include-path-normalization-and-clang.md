# Cpp idk include path normalization and clang format enforcement

- **Learned from:** human review on CL I10a869eef9a52f979795511e5e0e5708fd7429c2, patchset 1 (task `network-device-log`)
- **Date:** 2026-10-26
- **Changed:** `checks/build_only_scope.sh`

## Root cause

When migrating C/C++ targets that depend on an idk_cc_source_library with
include_base = "//sdk", headers included via checkout-root-relative angle
brackets (#include <sdk/lib/...>) compile in GN because default_include_dirs
adds -I//, whereas Bazel only adds -iquote . for the workspace root and exposes
the IDK library under -isystem sdk (<lib/...>). Rewriting #include <sdk/lib/...>
to #include <lib/...> changes alphabetical #include ordering, triggering Gerrit
Format/ClangFormat warnings when clang-format is not run, while build_only_scope
previously rejected all C/C++ file edits unconditionally without checking
clang-format on narrow include-path normalizations.

## Why not a one-off fix

Formatting only the single header in the current task would not prevent future
C/C++ migrations that normalize <sdk/lib/...> includes to <lib/...> from either
being blocked by build_only_scope or uploaded to Gerrit with unsorted #include
directives that fail Format/ClangFormat.
