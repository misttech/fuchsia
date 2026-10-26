# Rdep discovery worktree gitignore and path normalization

- **Learned from:** inner loop friction on a local task (task `src_developer_ffx_lib_doctor_utils`)
- **Date:** 2026-10-23
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

In visibility_audit.sh and find_rdeps.sh, discover_referencing_files preferred
running rg on workdir before falling back to git grep. When planter runs checks
on the host against a git worktree under .jiri_root/worktrees/<slot> (where .git
is a gitfile rather than a directory and the parent checkout's .gitignore
ignores /.jiri_root/ and sub-repositories), rg respects the parent .gitignore,
finds zero matching build files (exit status 1), sets used_rg = True, and skips
the per-repo git grep fallback, causing every legitimate caller in visibility to
be flagged as false_positive_rdep_visibility. Additionally, passing an absolute
workdir-prefixed path to find_rdeps.sh or visibility_audit.sh stripped only
leading slashes instead of normalizing relative to workdir.

## Why not a one-off fix

Any migration task evaluated in a .jiri_root/worktrees/<slot> worktree on a host
with ripgrep installed, or any package with callers in sub-repositories ignored
by the root .gitignore, will suffer identical false_positive_rdep_visibility
errors unless discover_referencing_files uses per-repository git grep first (and
passes --no-ignore-parent/--no-require-git across all repo roots when falling
back to rg) and normalizes workdir-prefixed target directory arguments.
