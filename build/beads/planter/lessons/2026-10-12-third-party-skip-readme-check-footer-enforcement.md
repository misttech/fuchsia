# Third party skip readme check footer enforcement

- **Learned from:** cq failure on CL I51caab408af59cdf4692100398b98c7e1922d06a, patchset 1 (task `pkg-merkle`)
- **Date:** 2026-10-12
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

The Fuchsia shac check `third_party_readme_update` rejects any commit that
modifies files under `third_party/<dep>` (outside `third_party/rust_crates/`)
with an enclosing `README.fuchsia` unless that `README.fuchsia` is also modified
or the commit message includes a `Skip-Readme-Check: <reason>` footer. Because
`build_only_scope` forbids modifying `README.fuchsia` during build-only
migrations and `commit_message_format` only checked `Bazel-Migration-Target:`
and `Test:` footers, migrations touching `third_party/` build files passed local
checks without `Skip-Readme-Check:` and failed `static-checks` in CQ.

## Why not a one-off fix

Adding `Skip-Readme-Check:` to only the single commit message in the current
task would leave all future GN-to-Bazel migrations that touch build files under
`third_party/<dep>` (directly or when migrating transitive third-party
dependencies) vulnerable to the exact same `third_party_readme_update` CQ
static-checks failure.
