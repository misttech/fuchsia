---
name: split-fuchsia-rust-dep-updates
description: >-
  Splits a Fuchsia Rust dependency update commit into two commits: one with pure
  copies/moves of vendored crates, and one with actual changes and migrations.
  Similar to Gerrit review splitting. Use when preparing a Rust dependency
  upgrade CL in Fuchsia where vendored crates are added, deleted, or updated,
  to make the diff reviewable.
---

# Splitting Fuchsia Rust Dependency Updates

Splits a dependency update commit into two parts:
1.  **Pure copies/moves**: Git sees them as 100% similar, making the diff clean.
1.  **Actual changes**: Contains content updates, Cargo.toml/Cargo.lock updates, and code migrations.

This structure makes it much easier for reviewers in Gerrit to see what actually changed in the dependency upgrade, rather than being overwhelmed by a huge diff of new vendored code.

## Workflow Overview

1.  Identify which crates are completely upgraded (moves) and which have multiple versions kept (copies).
1.  Run the helper script to automate the split.
1.  Verify the split was successful.

---

## 1. Automated Splitting

Use the bundled Python script to automate the entire split process. The script will handle the checkout of the base commit, perform the copies and moves, commit them, revert them, cherry-pick the target commit, and squash them into the correct structure.

Run the script from the root of your Fuchsia repository:

```bash
python3 third_party/rust_crates/skills/split_fuchsia_rust_dep_updates/scripts/split_updates.py \
  --repo-path=$(git rev-parse --show-toplevel) \
  --target-commit={commit_or_branch_to_split} \
  --base-commit={base_commit_usually_origin_main}
```

### Options

Flag | Default | Description
--- | --- | ---
`--repo-path` | (Required) | Absolute path to the local Fuchsia repository. Use `$(git rev-parse --show-toplevel)` to automatically detect it.
`--target-commit` | `HEAD` | The commit (or branch) containing the combined updates and migrations that you want to split.
`--base-commit` | `origin/main` | The base commit before the updates were applied.

### Success Criteria

Upon success, the script will output two commit hashes:
*   **Commit 1 (copies/moves)**: Contains only directory moves and copies under `third_party/rust_crates/vendor/`.
*   **Commit 2 (actual updates)**: Contains the rest of the changes.

You will be left in a detached HEAD state at Commit 2. Update your branch to this state:

```bash
git checkout -B {your_branch_name}
```

---

## 2. Verification

Verify that the split did not introduce any changes compared to the original target state. The diff between your new HEAD (Commit 2) and the original target commit must be empty:

```bash
git diff {original_target_commit} HEAD
```

If the diff is empty, the split was successful and the code is identical to your original working state.

---

## 3. Manual Steps (Fallback)

If the script fails (e.g., due to cherry-pick conflicts), you can perform the steps manually:

1.  **Identify copies and moves**:
    *   Compare `origin/main` (base) and `HEAD` (target).
    *   If both old and new versions exist in target: it is a **copy** (e.g. `aes-0.8.4` -> `aes-0.9.1`).
    *   If only the new version exists in target: it is a **move** (e.g. `chacha20-0.9.1` -> `chacha20-0.10.0`).

1.  **Checkout base**:
    ```bash
    git checkout origin/main
    ```

1.  **Apply copies**:
    For each copy `old` -> `new`:
    ```bash
    rm -rf third_party/rust_crates/vendor/{new}
    cp -pr third_party/rust_crates/vendor/{old} third_party/rust_crates/vendor/{new}
    git add third_party/rust_crates/vendor/{new}
    ```

1.  **Apply moves**:
    For each move `old` -> `new`:
    ```bash
    git checkout origin/main -- third_party/rust_crates/vendor/{old}
    rm -rf third_party/rust_crates/vendor/{new}
    git add third_party/rust_crates/vendor/{old}
    git mv third_party/rust_crates/vendor/{old} third_party/rust_crates/vendor/{new}
    ```

1.  **Commit copies/moves**:
    ```bash
    git commit -m "[rust-3p] Copy and move vendored crates"
    ```

1.  **Revert copies/moves**:
    ```bash
    git revert HEAD --no-edit
    ```

1.  **Cherry-pick target**:
    ```bash
    git cherry-pick {target_commit}
    ```
    Resolve any conflicts.

1.  **Squash the revert and cherry-pick from before into one commit**:
    ```bash
    git reset --soft HEAD~2
    git commit -C {target_commit}
    ```
