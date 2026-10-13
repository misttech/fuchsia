# Bazel host tool and bazel input file decommissioning

- **Learned from:** inner loop friction on CL I204d0e1985edb489eb73e1e34bfe5f49cc6924b0, patchset 6 (task `dmlc`)
- **Date:** 2026-10-13
- **Changed:** `checks/bazel_minimality.sh`, `checks/gn_target_type_parity.sh`

## Root cause

When migrating a host tool or generator target previously bridged into Bazel via
bazel_input_file() and @gn_targets// labels, two checks conflicted with the
required migration steps: (1) gn_target_type_parity rejected replacing
bazel_input_file()/bazel_input_directory() with an empty soft-transition group()
in BUILD.gn (and did not flag keeping bazel_input_file() alongside
bazel_host_tool() or leaving stale @gn_targets// references under build/bazel/),
and (2) bazel_minimality rejected updating shared .bzl rule definitions under
build/bazel/rules/ even when those files referenced the migrated package via
@gn_targets// at the change base and needed to switch to the native Bazel target
label.

## Why not a one-off fix

Any host binary or build generator previously exposed to Bazel rules through
bazel_input_file() and @gn_targets// labels across the tree requires replacing
the obsolete bazel_input_file() with an empty group() (or removing it) and
updating its @gn_targets// consumers in shared .bzl or BUILD.bazel files to
reference the migrated Bazel target directly; handling this pattern generically
in gn_target_type_parity.sh and bazel_minimality.sh prevents both false-positive
blocks and stale @gn_targets// bridges on all future host-tool migrations.
