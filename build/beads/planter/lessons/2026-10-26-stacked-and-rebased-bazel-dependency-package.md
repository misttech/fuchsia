# Stacked and rebased bazel dependency package detection

- **Learned from:** cq failure on CL I4b835ab2d696fc7a345394547335937da73039a8, patchset 5 (task `src-developer-ffx-lib-discovery`)
- **Date:** 2026-10-26
- **Changed:** `checks/cq_reachability.sh`

## Root cause

checks/cq_reachability.sh computed stacked_parent_files using a two-dot git diff
between origin/main and effective_diff_base (`git diff --name-only origin_main
effective_diff_base`), which when origin/main advances ahead of HEAD~1 falsely
treats every file modified on origin/main since HEAD~1 as a stacked parent
commit (flooding the coder with spurious stacked_bazel_dependency_package
findings for unrelated upstream crate rolls), and lacked an explicit
rebased_bazel_dependency_package INFO diagnostic when a `Depends on:` Bazel
dependency that caused an earlier CQ `no such package` failure has since landed
in origin/main and HEAD~1 via rebase.

## Why not a one-off fix

Fixing or re-uploading only a single package directory leaves cq_reachability.sh
misclassifying upstream origin/main commits as stacked parent commits whenever
origin/main moves ahead of HEAD~1 and leaves the coder without clear diagnostics
when a stacked `Depends on:` dependency lands on origin/main across any future
migration task.
