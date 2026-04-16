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
import os
import sys

import cartfs
import logger
import util
import workspace


def prepare_workspace_instance(
    snapshot: bool,
    use_local_mock_cartfs: bool,
) -> workspace.Workspace | None:
    """Prepares a workspace instance."""
    logger.emit_status("Preparing workspace instance...")
    # Attempt to identify the current cog and associated cartfs workspace.
    try:
        workspace_instance = workspace.Workspace.create(use_local_mock_cartfs)
    except workspace.NotInCogWorkspaceError:
        logger.log_error("This script can only be run in cog workspaces.")
        logger.log_error(
            "Please refer to https://go/fuchsia-cog-user-guide for instructions on fuchsia development with cog."
        )
        return None
    except cartfs.CartfsError as e:
        logger.log_exception(e)
        return None

    logger.log_info(f"Found repository: {workspace_instance.repo_dir}")
    logger.log_info(
        f"Found cartfs mount point: {workspace_instance.cartfs_instance.mount_point}"
    )

    # No need to reinitialize our cartfs workspace.
    if workspace_instance.has_cartfs_dir():
        logger.log_info(
            f"Workspace is already linked to cartfs: {workspace_instance.cartfs_dir}"
        )
        return workspace_instance

    # Attempt to snapshot the cartfs workspace from a previous instance.
    cartfs_dir = None
    if snapshot:
        logger.log_info(
            "Workspace is not linked to cartfs. Attempting to Snapshot from previous instance."
        )
        if not use_local_mock_cartfs:
            cartfs_dir = workspace_instance.snapshot_from_previous_instance()
        if not cartfs_dir:
            logger.log_info(
                "Unable to snapshot from previous instance. Creating a new"
                " cartfs workspace directory instead."
            )

    # Initialize an empty cartfs workspace directory.
    if not cartfs_dir:
        cartfs_dir = (
            workspace_instance.create_empty_cartfs_workspace_directory()
        )

    workspace_instance.link_to_cartfs(cartfs_dir)
    return workspace_instance


def _parse_args() -> argparse.Namespace:
    """Parses command-line arguments."""
    parser = argparse.ArgumentParser(
        description="Set up a cog-based workspace for Fuchsia development."
    )
    parser.add_argument(
        "--no-snapshot",
        dest="snapshot",
        action="store_false",
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
    return parser.parse_args()


def main() -> int:
    """Main function to set up the cog workspace."""
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

    if not util.check_gcert_status():
        logger.log_error("You do not have a valid gcert certificate.")
        logger.log_error("Please run 'gcert' and try again.")
        return 1

    workspace_instance = prepare_workspace_instance(
        args.snapshot, args.use_local_mock_cartfs
    )
    if not workspace_instance:
        logger.log_warn("Could not create workspace instance.")
        return 1

    if workspace_instance.is_checkout_uptodate():
        logger.log_info(
            "CartFS checkout is up to date, skipping cartfs initialization."
        )
    else:
        workspace_instance.checkout_cartfs_to_cog_revisions()
    return 0


if __name__ == "__main__":
    sys.exit(main())
