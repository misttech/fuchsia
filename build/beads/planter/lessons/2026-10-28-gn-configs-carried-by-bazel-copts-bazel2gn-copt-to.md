# GN configs carried by Bazel copts (bazel2gn copt-to-config translation) flagged as dropped attributes

- **Learned from:** inner loop friction on a local task (task `b2-cam1`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

The review step compares GN and Bazel attribute names, so a GN `configs` entry
looks dropped whenever the Bazel target has no `configs`, even when bazel2gn
produces it from `copts` (its built-in copt-to-config list, or a raw_overwrite
on copts). gn_attr_parity only marked configs as verified for build_flags()
targets, so for C/C++ libraries the warning stayed, and the coder could not
clear it without breaking the config bazel2gn already emits correctly.

## Why not a one-off fix

Any C/C++ library that disables warnings through mapped copts such as
-Wno-vla-cxx-extension, -Wno-conversion or -Wno-implicit-fallthrough, or names
package configs with a raw_overwrite on copts, hits the same stuck warning.
Fixing one package would not stop this in later migrations, and the coder cannot
fix it in BUILD files without breaking parity.
