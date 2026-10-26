# Commit message partial directory migration specificity

- **Learned from:** human review on CL Ibc0d845f2dfbfc59f6eb84a86d62a14485a6b368, patchset 2 (task `pkg-tests-pkg-resolver-resources`)
- **Date:** 2026-10-21
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

checks/commit_message_format.sh validated that 'Also migrated in this change:'
bullets and commit subjects used checkout-relative '//<dir>' paths, and even
instructed the coder to write bare '- //<dir>' bullets, without checking whether
'<dir>/BUILD.gn' still retained unmigrated GN targets. Consequently, when a
change only exported a shard or migrated specific resource/manifest targets in a
directory while leaving libraries, tests, or packages in BUILD.gn, the commit
message listed the bare '//<dir>' path and falsely implied the entire directory
was migrated.

## Why not a one-off fix

Fixing only the commit message for this single task leaves
checks/commit_message_format.sh accepting bare '- //<dir>' bullets and bare
subject directories whenever any future migration co-migrates only specific
targets or exports_files() entries in a partially migrated package.
