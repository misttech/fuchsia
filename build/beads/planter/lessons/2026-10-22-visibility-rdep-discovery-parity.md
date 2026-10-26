# Visibility rdep discovery parity

- **Learned from:** inner loop friction on a local task (task `src_lib_product_description`)
- **Date:** 2026-10-22
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

checks/visibility_audit.sh relied exclusively on calling rg with
subprocess.check_output and swallowed FileNotFoundError when rg was not
installed in the environment, leaving matched_files empty except for ancestor
BUILD.gn files. Meanwhile tools/find_rdeps.sh fell back to git grep when rg was
absent, so find_rdeps.sh discovered external reverse-dependency callers and
recommended their visibility while visibility_audit.sh saw zero reverse
dependencies and rejected the recommended visibility with
false_positive_rdep_visibility.

## Why not a one-off fix

Every migration task running in an environment without rg (or with callers in
sub-repositories ignored by the top-level .gitignore or in newly created
untracked BUILD.bazel files) would hit the same contradiction between
find_rdeps.sh and visibility_audit.sh unless both scripts use git grep plus git
ls-files across all repository roots.
