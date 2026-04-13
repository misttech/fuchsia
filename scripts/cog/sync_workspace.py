#!/usr/bin/env python3
# allow-non-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Syncs changes between Cog <=> CartFS checkouts."""

import argparse
import concurrent.futures
import enum
import hashlib
import json
import logging
import os
import re
import shutil
import sys
import textwrap
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

import logger
import workspace


class SyncError(workspace.WorkspaceError):
    """Error raised during sync operations."""


class WorkspaceType(enum.Enum):
    COG = "cog"
    CARTFS = "cartfs"


@dataclass
class SyncResult:
    added: set[str] = field(default_factory=set)
    modified: set[str] = field(default_factory=set)
    deleted: set[str] = field(default_factory=set)
    noop: set[str] = field(default_factory=set)
    failed: set[str] = field(default_factory=set)


class WorkspaceSyncService:
    """Syncs changes between Cog <=> CartFS checkouts."""

    def __init__(self) -> None:
        self.git_citc_path = shutil.which("git-citc")
        if not self.git_citc_path:
            raise workspace.WorkspaceError(
                "git-citc command not found in PATH."
            )

        self.workspace = workspace.Workspace.create()
        self.cog_root = self.workspace.workspace_dir
        self.cartfs_root = self.workspace.cartfs_directory
        if not self.cartfs_root:
            raise workspace.WorkspaceError(
                "No associated CartFS workspace found. "
                "Please run `//scripts/cog/setup_cog_workspace.py` first."
            )
        logger.log_info(f"Cog root: {self.cog_root}")
        logger.log_info(f"CartFS root: {self.cartfs_root}")

    @property
    def _config(self) -> dict[str, Any]:
        return self.workspace.config

    def _git_citc(self, *command: str, cwd: Path | None = None) -> str:
        assert self.git_citc_path is not None
        return self.workspace._run(
            [self.git_citc_path] + list(command),
            cwd=cwd or self.cog_root,
            capture_output=True,
        )

    @property
    def _affected_cog_repos(self) -> set[str]:
        return set(
            repo
            for repo in self._git_citc("api.get-modified-repos")
            .strip()
            .split("\n")
            if repo and repo not in self._config["repo"]["ignored"]
        )

    def _get_cog_commit(self, repo: str) -> str:
        """Returns the commit hash of a Cog repository."""
        stdout = self._git_citc("api.get-repo-states", repo)
        matches = re.findall(r"base_commit_hash:\s*\"?([^\s\"]+)\"?", stdout)
        if len(matches) != 1:
            raise workspace.WorkspaceError(
                f"Expected 1 match for base_commit_hash, got {len(matches)}:\n{stdout}"
            )
        return matches[0]

    @property
    def _cog_transfer_file_hashes(self) -> dict[str, str | None]:
        assert self.cartfs_root is not None
        file = self.cartfs_root / "cog_transfer_file_hashes.json"
        if not file.exists():
            return {}
        try:
            return json.loads(file.read_text())
        except (json.JSONDecodeError, OSError) as e:
            logger.log_error(f"Failed to read and parse {file}: {e}")
            return {}

    def affected_files(self, workspace_type: WorkspaceType) -> set[str]:
        """Returns the set of files different from the base commit in the given workspace."""
        if workspace_type == WorkspaceType.COG:

            def _get_affected_files(repo: str) -> set[str]:
                stdout_lines = self._git_citc(
                    "cli.diff",
                    "--patch=false",
                    self._get_cog_commit(repo),
                    "@",
                    cwd=self.cog_root / repo,
                ).split("\n")
                return {
                    f'{repo}/{line.split(" ", 1)[1]}'
                    for line in stdout_lines
                    if re.match(r"^\[.\] .+$", line)
                }

            affected_cog_repos = self._affected_cog_repos
            if not affected_cog_repos:
                return set()

            with concurrent.futures.ThreadPoolExecutor() as executor:
                return set.union(
                    *executor.map(_get_affected_files, affected_cog_repos)
                )

        # This technically isn't correct since we ideally want to track the files between
        # COG_BASE..CARTFS_HEAD, but instead this tracks COG_BASE..COG_HEAD from the last
        # Cog => CartFS sync, which is reflected in `cog_transfer_file_hashes.json`.
        #
        # Tangibly, this means that if `fx ...` subcommands modify files that aren't captured in
        # COG_BASE..COG_HEAD, then those files won't be synced back from CartFS => Cog.
        #
        # TODO(https://fxbug.dev/501540419): We should consider using `git` porcelain commands to
        # get the set of files changed between CARTFS_BASE..CARTFS_HEAD, as CARTFS_BASE is
        # approximately COG_HEAD. However, this will introduce extra complexity since:
        # 1. Currently, Cog-relative file paths are the source of truth and we map these to CartFS
        #    via `_cartfs_path`. Querying `git` from CartFS means we'll need some way to map
        #    CartFS-relative file paths to Cog-relative file paths.
        # 2. We'll need to account for git submodules.
        if workspace_type == WorkspaceType.CARTFS:
            return set(self._cog_transfer_file_hashes.keys())

        raise workspace.WorkspaceError(
            f"Unsupported workspace type: {workspace_type}"
        )

    def _md5hash(self, path: Path) -> str | None:
        try:
            if path.is_symlink():
                return hashlib.md5(os.readlink(path).encode()).hexdigest()

            if not path.is_file():
                return None

            with open(path, "rb") as f:
                file_hash = hashlib.md5()
                while chunk := f.read(8192):
                    file_hash.update(chunk)
                return file_hash.hexdigest()
        except OSError as e:
            logger.log_error(f"Failed to calculate hash for {path}: {e}")
            return None

    def _cog_path(self, path: str) -> Path:
        return self.cog_root / path

    def _cartfs_path(self, path: str) -> Path:
        assert self.cartfs_root is not None
        # Map Cog fuchsia dir to CartFS fuchsia dir.
        assert self._config["repo"]["fuchsia"]
        fuchsia_prefix = f'{self._config["repo"]["fuchsia"]}/'
        if path.startswith(fuchsia_prefix):
            path = path[len(fuchsia_prefix) :].lstrip("/")
            return self.cartfs_root / "fuchsia" / path

        # Map Cog integration dir (if it exists) to CartFS integration dir.
        integration_prefix = (
            self._config["repo"]["integration"]
            and f'{self._config["repo"]["integration"]}/'
        )
        if integration_prefix and path.startswith(integration_prefix):
            path = path[len(integration_prefix) :].lstrip("/")
            return self.cartfs_root / "integration" / path

        # Strip the repo-configured prefix and nest within the configured destination subdirectory.
        strip_prefix = self._config["repo"]["stripSrcPrefix"]
        if not path.startswith(strip_prefix):
            raise SyncError(
                f"Expected path {path} to start with strip prefix '{strip_prefix}'"
            )
        return (
            self.cartfs_root
            / self._config["repo"]["destSubdir"]
            / path[len(strip_prefix) :].lstrip("/")
        )

    def sync_batch(
        self,
        src_func: Callable[[str], Path],
        dest_func: Callable[[str], Path],
        paths: set[str],
        hash_func: Callable[[Path], str | None],
    ) -> SyncResult:
        result = SyncResult()
        for path in paths:
            try:
                src = src_func(path)
                dest = dest_func(path)
            except Exception as e:
                raise SyncError(f"Failed to resolve path '{path}'") from e

            logger.log_debug(f"Syncing path '{path}': {src} -> {dest}")

            # `git citc cli.diff` only reports files, not directories.
            if (src.is_dir() and not src.is_symlink()) or (
                dest.is_dir() and not dest.is_symlink()
            ):
                result.failed.add(path)
                continue

            # Handle deleted files.
            if not src.exists() and not src.is_symlink():
                if dest.exists() or dest.is_symlink():
                    # Attempt to delete the file. Note that deleting files from a Cog destination
                    # currently fails and will be caught below.
                    # We don't expect sync file deletions initiated from CartFS often.
                    try:
                        dest.unlink()
                        result.deleted.add(path)
                    except OSError as e:
                        logger.log_error(
                            f"Failed to apply deletion of '{dest.name}': {e}"
                        )
                        result.failed.add(path)
                else:
                    result.noop.add(path)
                continue

            # Handle added and modified files.
            # Only write to CartFS destinations if the file contents have changed to avoid updating
            # the file's `mtime` which unnecessarily invalidates incremental rebuilds.
            if hash_func(src) == hash_func(dest):
                result.noop.add(path)
                continue

            try:
                dest.parent.mkdir(parents=True, exist_ok=True)
                result_field = (
                    result.modified if dest.exists() else result.added
                )
                if dest.is_symlink() or (src.is_symlink() and dest.exists()):
                    dest.unlink()
                shutil.copy2(src, dest, follow_symlinks=False)
                result_field.add(path)
            except OSError as e:
                logger.log_error(f"Failed to sync '{dest.name}': {e}")
                result.failed.add(path)
        return result

    def sync_cog_to_cartfs(self) -> SyncResult:
        """Syncs changes from Cog to CartFS."""
        assert self.cartfs_root is not None
        self.workspace.checkout_cartfs_to_cog_revisions()
        cog_affected_files = self.affected_files(WorkspaceType.COG)
        cartfs_affected_files = self.affected_files(WorkspaceType.CARTFS)
        all_affected_files = cog_affected_files | cartfs_affected_files
        sync_result = self.sync_batch(
            self._cog_path, self._cartfs_path, all_affected_files, self._md5hash
        )

        # Keep track of the checkout files in CartFS that differ from the Cog base.
        # This helps apply any reverted Cog file modifications that are omitted by
        # `git citc cli.diff` in the next Cog => CartFS sync.
        #
        # Note: This approach is less accurate than using `git` since it cannot account for `fx`
        # commands that modify CartFS checkout files that aren't part of the Cog changes.
        # However, it is faster and significantly less complex when accounting for
        # reversing Cog => CartFS file path transformations and git submodules.
        (self.cartfs_root / "cog_transfer_file_hashes.json").write_text(
            json.dumps(
                {
                    path: self._md5hash(self._cartfs_path(path))
                    for path in cog_affected_files | sync_result.failed
                }
            )
        )
        return sync_result

    def sync_cartfs_to_cog(
        self, diff_against_previous_cog_to_cartfs_sync: bool
    ) -> SyncResult:
        """Syncs changes from CartFS to Cog."""
        if diff_against_previous_cog_to_cartfs_sync:
            previous_transfer_file_hashes = {
                self._cog_path(path): hash_val
                for path, hash_val in self._cog_transfer_file_hashes.items()
            }

            def _hash_func(path: Path) -> str | None:
                actual_hash = self._md5hash(path)
                # Only previously synced Cog paths will have their hashes overridden. CartFS paths
                # will purposely miss the dictionary lookup and return their actual_hash.
                try_cache_hash = previous_transfer_file_hashes.get(
                    path, actual_hash
                )
                if actual_hash != try_cache_hash:
                    logger.log_info(
                        f"Detected Cider edits to '{path.name}'. Treating its base hash as "
                        f"'{try_cache_hash}' to prevent accidental overwrite."
                    )
                return try_cache_hash

        else:
            _hash_func = self._md5hash

        return self.sync_batch(
            self._cartfs_path,
            self._cog_path,
            self.affected_files(WorkspaceType.CARTFS),
            _hash_func,
        )


def _parse_args() -> argparse.Namespace:
    """Parses command-line arguments."""
    parser = argparse.ArgumentParser(
        description="Syncs changes between Cog and CartFS checkouts."
    )
    parser.add_argument(
        "--from",
        dest="src",
        choices=["cog", "cartfs"],
        required=True,
        help="The workspace to sync from.",
    )
    parser.add_argument(
        "--to",
        dest="dest",
        choices=["cog", "cartfs"],
        required=True,
        help="The workspace to sync to.",
    )
    parser.add_argument(
        "--unsafe-overwrite-cog-changes-since-last-sync",
        action="store_true",
        help=(
            "By default, when syncing from CartFS to Cog, we compare CartFS file changes against "
            "the snapshot of Cog file hashes generated from the last Cog => CartFS sync instead of "
            "the current Cog file contents. This helps us avoid destroying file edits made in "
            "Cider during a build or test, etc.\n"
            "Setting this flag will force the sync to compare CartFS file changes against the "
            "current Cog file contents instead.\n"
            "Only compatible with the `--from cartfs --to cog` sync direction."
        ),
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="count",
        default=0,
        help="Increase verbosity level (-v for INFO, -vv for DEBUG).",
    )
    parser.add_argument(
        "--enable-status-updates",
        action="store_true",
        help="Enable status updates.",
    )
    parser.add_argument(
        "--color",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Enable or disable color output.",
    )
    args = parser.parse_args()
    if args.src == args.dest:
        parser.error("`--from` and `--to` must be different.")
    return args


def main() -> int:
    """Main function to sync the Cog <=> CartFS workspaces."""
    args = _parse_args()

    if not args.color:
        os.environ["NO_COLOR"] = "1"

    if args.verbose == 0:
        log_level = logging.WARNING
    elif args.verbose == 1:
        log_level = logging.INFO
    else:
        log_level = logging.DEBUG

    logger.init_logger(
        level=log_level,
        colors=args.color,
        enable_status_updates=args.enable_status_updates,
    )

    try:
        sync_service = WorkspaceSyncService()
    except Exception as e:
        logger.log_error(
            "An unexpected error occurred when initializing the sync service."
        )
        logger.log_exception(e)
        return 1

    try:
        if args.src == "cog":
            logger.log_info("Syncing changes from Cog to CartFS...")
            sync_result = sync_service.sync_cog_to_cartfs()
            if sync_result.failed:
                logger.log_warn(
                    f"Failed to sync {len(sync_result.failed)} files from Cog to CartFS:\n"
                    f"{textwrap.indent(chr(10).join(sync_result.failed), '    ')}\n"
                    "Some source file changes made in Cider won't be made effective for this "
                    "`fx`/`ffx` command invocation!"
                )
                return 0
        else:
            logger.log_info("Syncing changes from CartFS to Cog...")
            sync_result = sync_service.sync_cartfs_to_cog(
                not args.unsafe_overwrite_cog_changes_since_last_sync
            )
            if sync_result.failed:
                logger.log_warn(
                    f"Failed to sync {len(sync_result.failed)} files from CartFS to Cog:\n"
                    f"{textwrap.indent(chr(10).join(sync_result.failed), '    ')}\n"
                    "Some source file changes made by `fx`/`ffx` tooling won't be reflected back "
                    "to Cider!"
                )
                return 0

        logger.log_info(
            "Sync complete.\n"
            f"Added files ({len(sync_result.added)}): {sorted(sync_result.added)}\n"
            f"Modified files ({len(sync_result.modified)}): {sorted(sync_result.modified)}\n"
            f"Deleted files ({len(sync_result.deleted)}): {sorted(sync_result.deleted)}\n"
            f"No-op files ({len(sync_result.noop)}): {sorted(sync_result.noop)}\n"
            f"Failed files ({len(sync_result.failed)}): {sorted(sync_result.failed)}"
        )
    except Exception as e:
        logger.log_error(
            f"An unexpected error occurred when syncing changes from {args.src} to {args.dest}."
        )
        logger.log_exception(e)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
