#!/usr/bin/env -S python3 -B
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

import cartfs
import cartfs_out_directory
import logger
import prebuilts
import workspace


def prepare_workspace_instance() -> workspace.Workspace | None:
    """Prepares a workspace instance."""
    try:
        workspace_instance = workspace.Workspace.create()
        logger.log_info(
            f"Found workspace dir: {workspace_instance.workspace_dir}"
        )
        logger.log_info(
            f"Found cartfs mount point: {workspace_instance.cartfs_instance.mount_point}"
        )
        logger.log_info(
            f"the repository name is: {workspace_instance.repo_name}"
        )

        if (
            existing_cartfs_workspace_dir := workspace_instance.cartfs_workspace_dir
        ):
            logger.log_info(
                f"Workspace is already linked to cartfs: {existing_cartfs_workspace_dir}"
            )
        else:
            logger.log_info(
                "Workspace is not linked to cartfs. Attempting to Snapshot from previous instance."
            )
            cartfs_workspace_dir = (
                workspace_instance.snapshot_from_previous_instance()
            )
            if not cartfs_workspace_dir:
                logger.log_info(
                    "Unable to snapshot from previous instance. Creating new"
                    " cartfs workspace directory."
                )
                cartfs_workspace_dir = (
                    workspace_instance.create_empty_cartfs_workspace_directory()
                )
            workspace_instance.link_to_cartfs(cartfs_workspace_dir)

        return workspace_instance
    except workspace.NotInCogWorkspaceError as e:
        logger.log_error("This script can only be run in cog workspaces.")
        logger.log_error(
            "Please refer to https://go/fuchsia-cog-user-guide for instructions on fuchsia development with cog."
        )
    except cartfs.CartfsError as e:
        logger.log_exception(e)

    return None


def _parse_args() -> argparse.Namespace:
    """Parses command-line arguments."""
    parser = argparse.ArgumentParser(
        description="Set up a cog-based workspace for Fuchsia development."
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="count",
        default=0,
        help="Increase verbosity level (-v for INFO, -vv for DEBUG).",
    )
    return parser.parse_args()


def main() -> int | None:
    """Main function to set up the cog workspace."""
    args = _parse_args()

    if args.verbose == 1:
        log_level = logging.INFO
    elif args.verbose >= 2:
        log_level = logging.DEBUG
    else:
        log_level = logging.WARNING

    logger.init_logger(level=log_level, colors=True)

    workspace_instance = prepare_workspace_instance()
    if not workspace_instance:
        logger.log_warn("Could not create workspace instance.")
        return 1

    # TODO: Move this logic into the workspace instance.
    prebuilts_manager = prebuilts.Prebuilts(
        str(workspace_instance.cartfs_workspace_dir),
        str(workspace_instance.workspace_dir),
        workspace_instance.workspace_name,
        workspace_instance.repo_name,
    )
    if not prebuilts_manager.is_jiri_bootstrapped():
        prebuilts_manager.bootstrap_jiri()

    prebuilts_manager.cartfs_structure_initialization()
    prebuilts_manager.fetch_prebuilts()
    prebuilts_manager.create_symlinks()

    # Install/update cartfs-backed out directory.
    cartfs_out_directory.CartfsOutDirectory(
        cog_workspace_dir=workspace_instance.workspace_dir
        / workspace_instance.repo_name,
        cartfs_workspace_dir=workspace_instance.cartfs_workspace_dir,
    ).apply()


if __name__ == "__main__":
    sys.exit(main())
