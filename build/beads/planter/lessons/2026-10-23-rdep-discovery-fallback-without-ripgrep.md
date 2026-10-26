# Rdep discovery fallback without ripgrep

- **Learned from:** inner loop friction on CL Iba38f5ca324168715993fd4672573507be2847aa, patchset 3 (task `src_sys_time_lib_push_source`)
- **Date:** 2026-10-23
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

checks/visibility_audit.sh relied exclusively on calling the external `rg`
(ripgrep) binary to discover build files referencing candidate packages and
silently swallowed FileNotFoundError when `rg` was absent from PATH, leaving
matched_files empty (aside from ancestor BUILD.gn files) and causing every valid
caller visibility grant to be flagged as false_positive_rdep_visibility while
tools/find_rdeps.sh succeeded via its git grep fallback.

## Why not a one-off fix

Exempting a single package or target name would leave checks/visibility_audit.sh
broken on every migration task executed in an environment without ripgrep
installed or inside a git worktree checkout with untracked caller build files;
adding a shared `git grep --untracked` fallback across repositories when `rg` is
unavailable fixes reverse-dependency discovery systemically.
