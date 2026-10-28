# Review false positives on correct dual-build conversions: template changes that bazel2gn forces, and per-platform deps in GN if/else branches carried by a Bazel select()

- **Learned from:** inner loop friction on a local task (task `virt-libs`)
- **Date:** 2026-10-27
- **Changed:** `checks/gn_attr_parity.sh`, `checks/gn_target_type_parity.sh`

## Root cause

The review step (diff engine) cannot be edited from the machinery. Planter drops
one of its findings only when a check has already reported an INFO finding with
the same category, file, line and message. gn_target_type_parity accepted the
cc_test_executable to executable conversion, but only wrote its own
cc_test_executable_converted category, so the review step's template_parity
ERROR still blocked. gn_attr_parity's dep_alignment coverage only compared
against the parsed GN view with nested branch blocks left out, so the review
step's last-branch-only view (Linux deps replacing the Fuchsia deps) never got a
matching INFO finding and blocked the coder.

## Why not a one-off fix

Any migration of a C++ test binary from cc_test_executable, and any binary that
sets deps separately in is_fuchsia/is_linux branches and becomes a Bazel
select(), hits the same blocking false positives. Changing one package cannot
fix them, because the generated GN is already correct. Only coverage findings
written by the checks stop the coder from getting stuck on the same disputes in
future tasks.
