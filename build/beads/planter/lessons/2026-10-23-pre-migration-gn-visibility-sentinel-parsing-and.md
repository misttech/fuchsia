# Pre migration gn visibility sentinel parsing and fx test group label validation

- **Learned from:** inner loop friction on CL If551a3c64703a2bcc7982992aa26b17f54a716bd, patchset 1 (task `src_sys_time_lib_time_util`)
- **Date:** 2026-10-23
- **Changed:** `checks/build_verification.sh`, `checks/commit_message_format.sh`, `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

First, get_orig_gn_visibility_scopes in visibility_audit.sh and find_rdeps.sh
stripped '#' comments before splitting on 'BAZEL2GN SENTINEL', deleting the '##
BAZEL2GN SENTINEL' marker and causing post-migration generated targets below the
sentinel to shadow pre-migration hand-written GN visibility scopes. Second,
commit_message_format.sh instructed the coder to pass GN group('tests') labels
('//<dir>:tests') to 'fx test', and build_verification.sh matched any
':<target>' label in a package if the package prefix had any entry in
tests.json, even though 'fx test --dry' only matches leaf test targets in
tests.json and fails on GN group('tests') labels.

## Why not a one-off fix

Fixing only a single package leaves visibility_audit and find_rdeps misparsing
BAZEL2GN SENTINEL across all multi-patchset dual-build migrations and leaves
commit_message_format and build_verification accepting or recommending 'fx test
//<dir>:tests' GN group labels that fail 'fx test --dry'.
