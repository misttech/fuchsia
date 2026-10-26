# Host platform constraints and bazel migration hygiene

- **Learned from:** human review on CL Ie928b600286ff293d47fafbc362d4261199f7482, patchset 2 (task `src_lib_transfer_manifest`)
- **Date:** 2026-10-22
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

While commit_message_format, visibility_audit, cq_reachability, and
bazel_minimality already enforce 72-character commit body wrapping, caller
visibility, CI test suite registration, and current-year copyright headers,
bazel_minimality lacked a check rejecting @platforms//host:constraints.bzl
(HOST_CONSTRAINTS) and verifying that library or binary targets guarded by if
(is_host) in BUILD.gn set target_compatible_with = HOST_OS_CONSTRAINTS.

## Why not a one-off fix

Fixing only a single package leaves other host-only library and tool migrations
vulnerable to using @platforms//host:constraints.bzl or dropping if (is_host)
platform guards; enforcing HOST_OS_CONSTRAINTS parity in bazel_minimality.sh
prevents this class of defect across all future migrations.
