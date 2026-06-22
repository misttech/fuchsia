#!/usr/bin/env python3
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Script to split a Fuchsia Rust dependency update commit.

It splits it into:
1. A commit with pure copies/moves of vendored crates.
2. A commit with the actual content changes and migrations.
"""

import argparse
import os
import re
import subprocess
import sys

def run(cmd, cwd=None):
    print(f"Running: {cmd}")
    try:
        result = subprocess.run(
            cmd, shell=True, check=True, capture_output=True, text=True, cwd=cwd
        )
        return result.stdout.strip()
    except subprocess.CalledProcessError as e:
        print(f"Error running command: {cmd}")
        print(f"Stdout: {e.stdout}")
        print(f"Stderr: {e.stderr}")
        raise

def get_crates(repo_path, ref):
    cmd = f"git ls-tree -r --name-only {ref} third_party/rust_crates/vendor/"
    output = run(cmd, cwd=repo_path)
    crates = set()
    for line in output.splitlines():
        parts = line.split('/')
        if len(parts) >= 4:
            crate_dir = parts[3]
            # Heuristic to check if it looks like crate-version
            if '-' in crate_dir:
                name, version = crate_dir.rsplit('-', 1)
                if re.match(r'^\d', version):
                    crates.add(crate_dir)
    return crates

def main():
    parser = argparse.ArgumentParser(description="Split Fuchsia Rust dep updates")
    parser.add_argument("--repo-path", required=True, help="Path to Fuchsia repo")
    parser.add_argument("--target-commit", default="HEAD", help="Commit to split")
    parser.add_argument("--base-commit", default="origin/main", help="Base commit")
    args = parser.parse_args()

    repo_path = args.repo_path
    if not os.path.isdir(repo_path):
        print(f"Error: {repo_path} is not a directory")
        sys.exit(1)

    # Verify repo is clean
    status = run("git status --porcelain", cwd=repo_path)
    if status:
        print("Error: Repository is not clean. Commit or stash changes first.")
        print(status)
        sys.exit(1)

    # Resolve refs to hashes
    target_hash = run(f"git rev-parse {args.target_commit}", cwd=repo_path)
    base_hash = run(f"git rev-parse {args.base_commit}", cwd=repo_path)

    print(f"Target commit: {target_hash}")
    print(f"Base commit: {base_hash}")

    old_crates = get_crates(repo_path, base_hash)
    new_crates = get_crates(repo_path, target_hash)

    # Group by package name
    def group_by_name(crates):
        grouped = {}
        for c in crates:
            name, version = c.rsplit('-', 1)
            grouped.setdefault(name, []).append(version)
        return grouped

    old_grouped = group_by_name(old_crates)
    new_grouped = group_by_name(new_crates)

    all_names = set(old_grouped.keys()) | set(new_grouped.keys())

    copies = []
    moves = []

    for name in sorted(all_names):
        old_vers = old_grouped.get(name, [])
        new_vers = new_grouped.get(name, [])

        if old_vers and new_vers:
            if old_vers != new_vers:
                # Version changed.
                if len(old_vers) == 1 and len(new_vers) == 1:
                    moves.append((name, old_vers[0], new_vers[0]))
                elif len(old_vers) == 1 and len(new_vers) > 1:
                    for new_v in new_vers:
                        if new_v not in old_vers:
                            copies.append((name, old_vers[0], new_v))
                elif len(old_vers) > 1 and len(new_vers) == 1:
                    pass
                else:
                    added_vers = set(new_vers) - set(old_vers)
                    removed_vers = set(old_vers) - set(new_vers)
                    if len(added_vers) == 1 and len(removed_vers) == 1:
                        moves.append((name, list(removed_vers)[0], list(added_vers)[0]))
                    elif len(added_vers) == 1 and len(removed_vers) == 0:
                        closest_old = sorted(old_vers)[-1]
                        copies.append((name, closest_old, list(added_vers)[0]))

    if not copies and not moves:
        print("No upgrades detected to split.")
        sys.exit(0)

    print(f"Detected {len(copies)} copies and {len(moves)} moves.")

    # Checkout base commit
    run(f"git checkout {base_hash}", cwd=repo_path)

    vendor_dir = "third_party/rust_crates/vendor"

    try:
        # Apply copies
        for name, old_v, new_v in copies:
            old_path = os.path.join(vendor_dir, f"{name}-{old_v}")
            new_path = os.path.join(vendor_dir, f"{name}-{new_v}")
            run(f"rm -rf {new_path}", cwd=repo_path)
            run(f"cp -r {old_path} {new_path}", cwd=repo_path)
            run(f"git add {new_path}", cwd=repo_path)

        # Apply moves
        for name, old_v, new_v in moves:
            old_path = os.path.join(vendor_dir, f"{name}-{old_v}")
            new_path = os.path.join(vendor_dir, f"{name}-{new_v}")
            run(f"git checkout {base_hash} -- {old_path}", cwd=repo_path)
            run(f"rm -rf {new_path}", cwd=repo_path)
            run(f"git add {old_path}", cwd=repo_path)
            run(f"git mv {old_path} {new_path}", cwd=repo_path)

        # Commit copies/moves
        run('git commit -m "[rust-3p] Copy and move vendored crates"', cwd=repo_path)
        commit1_hash = run("git rev-parse HEAD", cwd=repo_path)

        # Revert copies/moves
        run("git revert HEAD --no-edit", cwd=repo_path)

        # Cherry-pick target
        try:
            run(f"git cherry-pick {target_hash}", cwd=repo_path)
        except Exception:
            print("Cherry-pick failed. You may need to resolve conflicts manually.")
            print("Run 'git cherry-pick --abort' to cancel if needed.")
            raise

        # Squash revert and cherry-pick
        run("git reset --soft HEAD~2", cwd=repo_path)
        run(f"git commit -C {target_hash}", cwd=repo_path)
        commit2_hash = run("git rev-parse HEAD", cwd=repo_path)

        print("\nSuccess!")
        print(f"Commit 1 (copies/moves): {commit1_hash}")
        print(f"Commit 2 (actual updates): {commit2_hash}")
        print("You are now in detached HEAD at Commit 2.")
        print("To update your branch, run: git checkout -B <branch_name>")

    except Exception as e:
        print(f"Error occurred: {e}")
        print("Attempting to restore original state...")
        run(f"git checkout {target_hash}", cwd=repo_path)
        sys.exit(1)

if __name__ == "__main__":
    main()
