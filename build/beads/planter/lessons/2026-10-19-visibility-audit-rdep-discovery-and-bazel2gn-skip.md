# Visibility audit rdep discovery and bazel2gn skip gn scope

- **Learned from:** inner loop friction on a local task (task `pkg-testing-fake-ssl-certs`)
- **Date:** 2026-10-19
- **Changed:** `checks/visibility_audit.sh`

## Root cause

visibility_audit.sh relied exclusively on calling 'rg' to discover
reverse-dependency build files and silently swallowed FileNotFoundError when
'rg' was not installed in the environment (unlike find_rdeps.sh and
build_verification.sh, which fall back to 'git grep'), leaving per_target_rdeps
empty and falsely flagging valid visibility entries as
false_positive_rdep_visibility. Additionally, for targets skipped by bazel2gn
(such as '# @bazel2gn:skip' or resource() targets retained above the BAZEL2GN
SENTINEL in BUILD.gn), visibility_audit assumed bazel2gn overwrote the GN
target's visibility with the Bazel visibility list.

## Why not a one-off fix

Every migration running in an environment without 'rg' installed or migrating a
'# @bazel2gn:skip' / resource() target retained above the sentinel in BUILD.gn
hits the same false-positive rdep and GN visibility scope mismatch unless
visibility_audit.sh falls back to 'git grep' across checkout repositories and
only enforces converted GN visibility scopes on targets actually translated by
bazel2gn.
