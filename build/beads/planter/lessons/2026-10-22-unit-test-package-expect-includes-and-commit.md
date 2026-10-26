# Unit test package expect includes and commit trybot footer hygiene

- **Learned from:** human review on CL If6a81435bf8531155538049cfca7fc9444b5835f, patchset 1 (task `src_sys_time_timekeeper_persistence`)
- **Date:** 2026-10-22
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

First, migration_sanity.sh only treated newly added unit-test .cml files as
replaceable by fx_unittest_package when every shard in their include list
belonged to a fixed set of runner/syslog shards, missing shards backed by
GN/Bazel expect_includes() targets (such as tmp_storage.shard.cml) that
fx_unittest_package automatically includes when passed via aspect_hints on the
test binary. Second, commit_message_format.sh did not audit Cq-Include-Trybots:
footers, allowing unwarranted architecture-specific trybot footers on
architecture-independent migrations.

## Why not a one-off fix

Fixing only a single package leaves migration_sanity blind to other unit-test
packages whose generated manifests include expect_includes()-backed shards, and
allows future migrations to emit unrequested Cq-Include-Trybots: footers in
commit messages.
