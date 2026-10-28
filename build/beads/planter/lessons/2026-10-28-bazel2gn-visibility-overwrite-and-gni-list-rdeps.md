# Bazel2gn visibility overwrite and gni list rdeps

- **Learned from:** human review on CL I6cfabf00ecba0f03ecb2720d36e57639b83181c8, patchset 2 (task `src-developer-ffx-plugins-platform`)
- **Date:** 2026-10-28
- **Changed:** `checks/bazel_minimality.sh`, `checks/visibility_audit.sh`, `prompts/coder.md`, `tools/find_rdeps.sh`

## Root cause

First, `prompts/coder.md` previously instructed the coding agent to preserve
`visibility = [ ":*" ]` in GN by appending `# @bazel2gn:raw_overwrite:[ ":*" ]`
on `visibility`, and `checks/bazel_minimality.sh` did not flag `#
@bazel2gn:raw_overwrite` on `visibility` when the overwritten GN visibility
(`:*`, `./*`, `//<pkg>:*`, `//<pkg>/*`) is already produced natively by
`bazel2gn` (which automatically prepends `":*"` via
`ensureSamePackageVisibility` to non-empty visibility lists, maps
`"//visibility:private"` to `[ ":*" ]`, and maps `:__pkg__` / `:__subpackages__`
to `:*` / `/*`). Second, `checks/visibility_audit.sh` unconditionally rejected
`visibility = ["//visibility:private"]` even when used as the sole entry on a
`bazel2gn`-synced target to emit `visibility = [ ":*" ]` in `BUILD.gn`. Third,
`tools/find_rdeps.sh` and `checks/visibility_audit.sh` only expanded `.gni`
files that defined `template(...)`, missing `.gni` files that define shared
dependency list variables imported by multiple `BUILD.gn` packages.

## Why not a one-off fix

Fixing only the single target directory's `BUILD.bazel` leaves the misleading `#
@bazel2gn:raw_overwrite:[ ":*" ]` instruction in `prompts/coder.md`, allows
future dual-build migrations to attach redundant `@bazel2gn:raw_overwrite`
annotations to `visibility` instead of using idiomatic Bazel `visibility`
entries (or `"//visibility:private"` when `:*` alone is needed in GN), and
leaves `.gni` dependency list variables unexpanded in `find_rdeps` and
`visibility_audit` across all subsystems.
