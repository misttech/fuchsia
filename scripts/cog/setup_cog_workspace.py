#!/usr/bin/env python3
# allow-non-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""This script is used to set up a cog-based workspace for Fuchsia development.
It is currently highly experimental and not guaranteed to work.
"""

import argparse
import logging
import sys
from pathlib import Path

import cartfs
import logger
import workspace


def prepare_workspace_instance(
    disable_snapshot: bool,
    use_local_mock_cartfs: bool,
    repo_root: Path | None,
) -> workspace.Workspace | None:
    """Prepares a workspace instance."""
    # Attempt to identify the current cog and associated cartfs workspace.
    try:
        workspace_instance = workspace.Workspace.create(
            use_local_mock_cartfs, repo_root
        )
    except workspace.NotInCogWorkspaceError:
        logger.log_error("This script can only be run in cog workspaces.")
        logger.log_error(
            "Please refer to https://go/fuchsia-cog-user-guide for instructions on fuchsia development with cog."
        )
        return None
    except cartfs.CartfsError as e:
        logger.log_exception(e)
        return None

    logger.log_info(f"Found workspace dir: {workspace_instance.workspace_dir}")
    logger.log_info(
        f"Found cartfs mount point: {workspace_instance.cartfs_instance.mount_point}"
    )
    logger.log_info(f"Repository name: {workspace_instance.repo_name}")

    # No need to reinitialize our cartfs workspace.
    if workspace_instance.cartfs_directory:
        logger.log_info(
            f"Workspace is already linked to cartfs: {workspace_instance.cartfs_directory}"
        )
        return workspace_instance

    # Attempt to snapshot the cartfs workspace from a previous instance.
    cartfs_directory = None
    if not disable_snapshot:
        logger.log_info(
            "Workspace is not linked to cartfs. Attempting to Snapshot from previous instance."
        )
        if not use_local_mock_cartfs:
            cartfs_directory = (
                workspace_instance.snapshot_from_previous_instance()
            )
        if not cartfs_directory:
            logger.log_info(
                "Unable to snapshot from previous instance. Creating a new"
                " cartfs workspace directory instead."
            )

    # Initialize an empty cartfs workspace directory.
    if not cartfs_directory:
        cartfs_directory = (
            workspace_instance.create_empty_cartfs_workspace_directory()
        )

    workspace_instance.link_to_cartfs(cartfs_directory)
    return workspace_instance


def _parse_args() -> argparse.Namespace:
    """Parses command-line arguments."""
    parser = argparse.ArgumentParser(
        description="Set up a cog-based workspace for Fuchsia development."
    )
    parser.add_argument(
        "--no-snapshot",
        dest="disable_snapshot",
        action="store_true",
        help="Disable snapshotting and initialize this workspace from scratch.",
    )
    parser.add_argument(
        "-l",
        "--local-mock-cartfs",
        dest="use_local_mock_cartfs",
        action="store_true",
        help="Use a local mock cartfs directory located at ~/.mock_cartfs.",
    )
    parser.add_argument(
        "--repo-root",
        type=str,
        default=None,
        help="Specify the repository root directory. If not specified, the current directory will be used.",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="count",
        default=0,
        help="Increase verbosity level (-v for INFO, -vv for DEBUG).",
    )
    return parser.parse_args()


def main() -> int:
    """Main function to set up the cog workspace."""
    args = _parse_args()

    if args.verbose == 1:
        log_level = logging.INFO
    elif args.verbose >= 2:
        log_level = logging.DEBUG
    else:
        log_level = logging.WARNING

    logger.init_logger(
        level=log_level,
        colors=True,
    )

    repo_root = None
    if args.repo_root:
        repo_root = Path(args.repo_root)
        if not repo_root.is_dir():
            logger.log_error(
                f"The specified repo-root is not a valid directory: {args.repo_root}"
            )
            return 1
        repo_root = repo_root.resolve()

    workspace_instance = prepare_workspace_instance(
        args.disable_snapshot, args.use_local_mock_cartfs, repo_root
    )
    if not workspace_instance:
        logger.log_warn("Could not create workspace instance.")
        return 1

    workspace_instance.initialize_cartfs_directory()
    return 0


if __name__ == "__main__":
    sys.exit(main())
