# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""SCM and multi-repository discovery library for Fuchsia devshell tools."""

import collections
import dataclasses
import os
import pathlib
import subprocess
from collections.abc import Collection, Sequence


@dataclasses.dataclass(frozen=True)
class RepoGroup:
    """A Git repository target for SHAC execution.

    Attributes:
        repo_root: Absolute path to repository root (e.g. /fuchsia or
            /fuchsia/vendor/google).
        has_shac: True if repo_root contains shac.textproto.
        files: Relative path strings for target files (empty for default
            --git / --all modes).
    """

    repo_root: pathlib.Path
    has_shac: bool
    files: Sequence[str]


def _find_active_repositories(
    fuchsia_dir: pathlib.Path,
) -> Sequence[pathlib.Path]:
    """Discovers first-party Git repositories across the Fuchsia checkout.

    Performs a shallow inspection of $FUCHSIA_DIR and immediate subdirectories
    under $FUCHSIA_DIR/vendor/*/ containing a .git directory.
    Skips heavy directories (out/, third_party/) to complete in <2ms.

    Returns a deterministically sorted sequence with $FUCHSIA_DIR first,
    followed by vendor repositories sorted lexicographically by path.
    """
    fuchsia_dir = fuchsia_dir.resolve()

    # 1. Root repository
    if (fuchsia_dir / ".git").exists() or (fuchsia_dir / ".fx-root").exists():
        root_repos: Sequence[pathlib.Path] = (fuchsia_dir,)
    else:
        root_repos = ()

    # 2. Vendor repositories
    vendor_dir = fuchsia_dir / "vendor"
    if vendor_dir.is_dir():
        vendor_repos = tuple(
            sorted(
                entry.resolve()
                for entry in vendor_dir.iterdir()
                if entry.is_dir() and (entry / ".git").exists()
            )
        )
    else:
        vendor_repos = ()

    return (*root_repos, *vendor_repos)


def _check_has_shac(repo_root: pathlib.Path) -> bool:
    """Checks if a repository root contains a shac.textproto configuration."""
    return (repo_root / "shac.textproto").is_file()


def _get_repo_root_for_path(path: pathlib.Path) -> pathlib.Path:
    """Resolves the enclosing repository root for any arbitrary file or directory.

    Walks up parent directories checking for .git or .fx-root to find the
    innermost repository root (handling nested repositories and uncreated files
    without subprocess overhead), falling back to `git rev-parse --show-toplevel`.
    """
    if path.is_dir():
        current = path
    else:
        current = path.parent

    for candidate in (current, *current.parents):
        if (candidate / ".git").exists() or (candidate / ".fx-root").exists():
            return candidate.resolve()

    target_dir = current
    while not target_dir.exists() and target_dir != target_dir.parent:
        target_dir = target_dir.parent
    completed_process = subprocess.run(
        ["git", "-C", str(target_dir), "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
        check=True,
    )
    return pathlib.Path(completed_process.stdout.strip()).resolve()


def canonicalize_path(
    raw_path: str, cwd: pathlib.Path, fuchsia_dir: pathlib.Path
) -> pathlib.Path:
    """Normalizes user-supplied paths into absolute filesystem paths.

    Handles GN-style '//' prefixes (relative to $FUCHSIA_DIR) and relative paths
    (relative to current working directory). Resolves parent directory symlinks
    without dereferencing leaf symlinks.

    Args:
        raw_path: Raw path string provided by the user (e.g. '//src/foo.cc' or
            'foo.cc').
        cwd: Current working directory used to resolve relative paths.
        fuchsia_dir: Absolute path to the Fuchsia checkout root.

    Returns:
        Normalized absolute path with parent symlinks resolved and leaf symlinks
        preserved.
    """
    if raw_path.startswith("//"):
        if relative_path := raw_path[2:].lstrip("/"):
            path = fuchsia_dir / relative_path
        else:
            path = fuchsia_dir
    else:
        path = pathlib.Path(raw_path)
        if not path.is_absolute():
            path = cwd / path
    path = pathlib.Path(os.path.normpath(path))
    if path.is_symlink():
        return path.parent.resolve() / path.name
    else:
        return path.resolve()


def get_repo_groups(
    fuchsia_dir: pathlib.Path,
    cwd: pathlib.Path,
    explicit_files: Collection[str] | None = None,
) -> Sequence[RepoGroup]:
    """Discovers active repositories and partitions explicit files if provided.

    - In default (--git) or --all mode (when explicit_files is None):
      returns RepoGroups for all active repositories with empty `files=()`.
      SHAC handles all git diffing and file discovery internally.
    - In --files / --target mode (when explicit_files is not None): resolves
      each file's enclosing repository and populates RepoGroup.files with
      relative path strings for `shac -C <repo_root>`. If explicit_files is
      empty, returns ().

    Args:
        fuchsia_dir: Absolute path to the Fuchsia checkout root.
        cwd: Current working directory for resolving relative paths.
        explicit_files: Optional collection of user-specified file paths. If
            None, returns all active repositories for whole-repo checks.

    Returns:
        A deterministically sorted sequence of RepoGroups (root repo first,
        followed by other repos sorted lexicographically) with sorted,
        deduplicated `files` sequences.

    Raises:
        subprocess.CalledProcessError: If a target path is not inside any Git
            repository and `git rev-parse --show-toplevel` fails.
        FileNotFoundError: If a target path requires invoking `git` and the
            `git` executable cannot be found.
    """
    fuchsia_dir = fuchsia_dir.resolve()

    def repo_sort_key(group: RepoGroup) -> tuple[int, pathlib.Path]:
        if group.repo_root == fuchsia_dir:
            rootness_subkey = 0
        else:
            rootness_subkey = 1
        return (rootness_subkey, group.repo_root)

    if explicit_files is None:
        repos = {
            *_find_active_repositories(fuchsia_dir),
            _get_repo_root_for_path(cwd),
        }
        return tuple(
            sorted(
                (
                    RepoGroup(
                        repo_root=repo_root,
                        has_shac=_check_has_shac(repo_root),
                        files=(),
                    )
                    for repo_root in repos
                ),
                key=repo_sort_key,
            )
        )

    repo_files = collections.defaultdict(set)
    for raw_path in explicit_files:
        abs_path = canonicalize_path(raw_path, cwd, fuchsia_dir)
        repo_root = _get_repo_root_for_path(abs_path)

        if abs_path.is_relative_to(repo_root):
            repo_files[repo_root].add(str(abs_path.relative_to(repo_root)))

    return tuple(
        sorted(
            (
                RepoGroup(
                    repo_root=repo_root,
                    has_shac=_check_has_shac(repo_root),
                    files=tuple(sorted(files)),
                )
                for repo_root, files in repo_files.items()
            ),
            key=repo_sort_key,
        )
    )
